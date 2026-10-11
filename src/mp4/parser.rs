use std::collections::HashMap;

use super::boxes::{
    RawBox, Reader, child_boxes, fourcc, invalid_media, optional_child, required_child,
};
use super::codec;
use super::edit::{self, ElstEntry, TrackEdit};
use super::fragments::{self, TrackDefaults};
use super::protection;
use super::tables::{parse_sample_tables, sample_index};
use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{MediaIndex, SampleIndex, SkippedTrack, Track, TrackKey, TrackKind};
use crate::source::{Fragment, MediaSourceKind, Metadata, SourceIdentity};

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
    let metadata = Metadata::fetch(source, limits).await?;
    let identity = source.identity().clone();
    let limits_for_parse = limits.clone();
    let moov_range = metadata.moov_range();
    let moov = metadata.moov_shared();
    let parsing = tokio::task::spawn_blocking(move || {
        parse_metadata(&metadata, identity, &limits_for_parse).map(|index| (metadata, index))
    });
    // Mutation check: the object must be unchanged, and `moov` must read back the same. The
    // read-back runs while the tables are parsed, off the first viewer's critical path; a change
    // that lands after it is still caught by `verify_unchanged`, which runs after both. The bytes
    // parsed are still in memory, so they are compared directly rather than hashed again.
    let (parsed, unchanged) = tokio::join!(parsing, source.matches(moov_range, moov));
    let (metadata, index) = parsed.map_err(|error| Error::Io(std::io::Error::other(error)))??;
    source.verify_unchanged().await?;
    if !unchanged? {
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
    // Everything the index is built from. With no fragments this is the hash of `moov` alone.
    // `moov` is hashed once, BLAKE3: the `moov` hash is a copy of the hasher's state before any
    // fragment is added. A long file's `moov` is megabytes, so this is on the first viewer's
    // critical path; BLAKE3 hashes it in about a millisecond where SHA-256 took 15 to 18 ms on
    // a CPU without SHA instructions (docs/benchmarks.md, "Cold start").
    let hash = || {
        let mut hasher = blake3::Hasher::new();
        hasher.update(moov_bytes);
        let moov_hash: [u8; 32] = *hasher.clone().finalize().as_bytes();
        for fragment in metadata.fragments() {
            hasher.update(&fragment.offset.to_be_bytes());
            hasher.update(&fragment.bytes);
        }
        let metadata_hash: [u8; 32] = *hasher.finalize().as_bytes();
        (moov_hash, metadata_hash)
    };
    if moov.tracks.len() > limits.max_tracks {
        return Err(Error::Unsupported(format!(
            "track count {} exceeds configured limit {}",
            moov.tracks.len(),
            limits.max_tracks
        )));
    }

    let source = if moov.fragmented {
        if metadata.fragments().is_empty() {
            return Err(invalid_media("the file is fragmented but has no fragments"));
        }
        TrackSource::Fragmented {
            fragments: metadata.fragments(),
            defaults: &moov.defaults,
        }
    } else if metadata.fragments().is_empty() {
        TrackSource::Tables
    } else {
        return Err(invalid_media("moof boxes without an mvex box in moov"));
    };

    let mut skipped_tracks = Vec::new();
    let mut kept = Vec::new();
    for raw in &moov.tracks {
        if let Disposition::Skip(reason) = raw.disposition {
            skipped_tracks.push(SkippedTrack {
                id: raw.id,
                handler: fourcc(raw.handler),
                reason,
            });
        } else {
            kept.push(raw);
        }
    }
    let (parsed, (moov_hash, metadata_hash)) = parse_tracks(
        &kept,
        moov.movie_timescale,
        metadata.len(),
        limits,
        &source,
        hash,
    );
    let mut tracks = Vec::with_capacity(parsed.len());
    let mut edits = Vec::with_capacity(parsed.len());
    for result in parsed {
        let (track, edit) = result?;
        tracks.push(track);
        edits.push(edit);
    }

    let timescales = edits
        .iter()
        .zip(&tracks)
        .map(|(edit, track)| (*edit, track.timescale))
        .collect::<Vec<_>>();
    let shifts = edit::timeline_shifts(&timescales, moov.movie_timescale)?;
    let presentation_offset_ms = edit::shared_offset_millis(&timescales, moov.movie_timescale);
    for ((track, edit), shift) in tracks.iter_mut().zip(edits).zip(shifts) {
        edit::apply(track, edit, shift)?;
    }
    if moov.fragmented {
        normalise_fragmented_timeline(&mut tracks)?;
    }
    assign_keys(&mut tracks);
    identity.moov_hash = Some(moov_hash);
    identity.metadata_hash = Some(metadata_hash);

    let duration = if moov.movie_duration == 0 {
        movie_duration(&tracks, moov.movie_timescale)
    } else {
        moov.movie_duration
    };
    Ok(MediaIndex {
        source: identity,
        movie_timescale: moov.movie_timescale,
        duration,
        presentation_offset_ms,
        tracks,
        skipped_tracks,
        fragmentation: moov.fragmented.then(|| crate::media::Fragmentation {
            fragments: metadata.fragments().len(),
            discovery: metadata.discovery(),
            dropped_tail: metadata.dropped_tail(),
        }),
    })
}

/// Indexes every kept track, in parallel, while `hash` runs on this thread.
///
/// A long file's tracks each have tens or hundreds of thousands of samples, and this is the
/// first viewer's wait. Each track gets a scoped thread and the hash stays on this one, so the
/// wait is the slowest of them rather than their sum. Results are returned in file order, so
/// the first error is the one a sequential parse would report.
fn parse_tracks<H: Send>(
    kept: &[&RawTrack<'_>],
    movie_timescale: u32,
    source_len: u64,
    limits: &LimitsConfig,
    source: &TrackSource<'_>,
    hash: impl FnOnce() -> H,
) -> (Vec<Result<(Track, TrackEdit)>>, H) {
    let parse_one =
        |raw: &&RawTrack<'_>| parse_track(raw, movie_timescale, source_len, limits, source);
    std::thread::scope(|scope| {
        let handles = kept
            .iter()
            .map(|raw| scope.spawn(move || parse_one(raw)))
            .collect::<Vec<_>>();
        let hashes = hash();
        let parsed = handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect::<Vec<_>>();
        (parsed, hashes)
    })
}

/// Moves a fragmented file's timeline to start at zero and gives each track the duration its
/// samples add up to.
///
/// Fragmented files are often stamped with wall-clock or stream time, so their first decode
/// time is far from zero, and their `mvhd`, `mdhd`, and `tkhd` durations are usually zero. The
/// earliest first decode time across tracks becomes the origin, so the tracks keep their
/// relative timing and every duration is unchanged. Tracks have different timescales, so the
/// earliest is found in seconds and converted into each track's ticks, rounding down so that no
/// track's first sample goes below zero.
fn normalise_fragmented_timeline(tracks: &mut [Track]) -> Result<()> {
    // The track whose first sample is earliest in seconds: a/ts_a < b/ts_b is a·ts_b < b·ts_a.
    let Some((earliest_ticks, earliest_timescale)) = tracks
        .iter()
        .filter_map(|track| Some((track.samples.first()?.decode_time, track.timescale)))
        .min_by(|(a, ts_a), (b, ts_b)| {
            (u128::from(*a) * u128::from(*ts_b)).cmp(&(u128::from(*b) * u128::from(*ts_a)))
        })
    else {
        return Ok(());
    };
    for track in tracks {
        let origin = u128::from(earliest_ticks) * u128::from(track.timescale)
            / u128::from(earliest_timescale);
        let origin =
            u64::try_from(origin).map_err(|_| invalid_media("timeline origin overflow"))?;
        // Rounded down from the earliest first sample, so no decode time goes below zero.
        track.samples = track
            .samples
            .shifted(-i128::from(origin), "timeline origin overflow")?;
        let last = track
            .samples
            .last()
            .ok_or_else(|| invalid_media("a fragmented track has no samples"))?;
        track.duration = last
            .decode_time
            .checked_add(u64::from(last.duration))
            .ok_or_else(|| invalid_media("track duration overflow"))?;
    }
    Ok(())
}

/// The movie duration in the movie timescale, from the longest track.
fn movie_duration(tracks: &[Track], movie_timescale: u32) -> u64 {
    tracks
        .iter()
        .map(|track| {
            u128::from(track.duration) * u128::from(movie_timescale) / u128::from(track.timescale)
        })
        .max()
        .map_or(0, |duration| u64::try_from(duration).unwrap_or(u64::MAX))
}

/// Where a track's samples come from.
enum TrackSource<'a> {
    /// The sample tables in `moov`.
    Tables,
    /// The `moof` boxes of a fragmented file, with the `trex` defaults.
    Fragmented {
        fragments: &'a [Fragment],
        defaults: &'a HashMap<u32, TrackDefaults>,
    },
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
    source: &TrackSource<'_>,
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
    // A protected entry is read as the codec it was before protection (`frma`).
    let sinf = if matches!(&entry.name, b"encv" | b"enca") {
        let children = codec::protected_entry_children(entry.name, entry.payload, raw.id)?;
        let sinf = required_child(children, *b"sinf")?;
        Some(protection::parse_sinf(raw.id, sinf.payload)?)
    } else {
        None
    };
    let format = sinf
        .as_ref()
        .map_or(entry.name, |sinf| sinf.original_format);
    let codec = match (&format, kind) {
        (b"avc1", TrackKind::Video) => codec::parse_avc(entry.payload)?,
        (b"hvc1" | b"hev1", TrackKind::Video) => codec::parse_hevc(entry.payload, format)?,
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
    let samples = match source {
        TrackSource::Tables => {
            sample_index(parse_sample_tables(stbl.payload)?, source_len, limits)?
        }
        TrackSource::Fragmented {
            fragments,
            defaults,
        } => {
            fragments::reject_mixed(raw.id, sample_count(stbl.payload)?)?;
            let track_defaults = defaults.get(&raw.id).copied().unwrap_or_default();
            SampleIndex::from_samples(&fragments::track_samples(
                fragments,
                raw.id,
                track_defaults,
                source_len,
                limits,
            )?)
        }
    };
    if samples.is_empty() {
        return Err(invalid_media(&format!("track {} has no samples", raw.id)));
    }
    let source_protection = match (sinf, source) {
        (Some(sinf), TrackSource::Tables) => {
            protection::read_protection(raw.id, &sinf, stbl.payload, samples.len())?
        }
        (Some(sinf), TrackSource::Fragmented { fragments, .. }) => {
            protection::read_fragmented_protection(
                raw.id,
                &sinf,
                stbl.payload,
                fragments,
                samples.len(),
            )?
        }
        (None, _) => None,
    };
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
            source_protection,
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
    /// True when `moov` has an `mvex`: the samples are in `moof` boxes, not in `moov`.
    fragmented: bool,
    /// The `trex` defaults by track ID, empty unless fragmented.
    defaults: HashMap<u32, TrackDefaults>,
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
    let defaults = match children.iter().find(|child| child.name == *b"mvex") {
        Some(mvex) => Some(fragments::parse_defaults(mvex.payload)?),
        None => None,
    };
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
        fragmented: defaults.is_some(),
        defaults: defaults.unwrap_or_default(),
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
        b"encv" | b"enca" => {
            // Whether the file can be read is decided when it is assembled, by whether the
            // mapper gave a key (TDD 0014); here the entry only has to say how it is protected.
            let children = codec::protected_entry_children(entry.name, entry.payload, track_id)?;
            if optional_child(children, *b"sinf")?.is_none() {
                return Err(Error::Unsupported(format!(
                    "track {track_id}: encrypted media without protection information is not supported"
                )));
            }
            Ok(Disposition::Package)
        }
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

    fn block_on<F: Future>(future: F) -> F::Output {
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
            for (sample, packet) in track.samples.all().iter().zip(expected_packets) {
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
        let video_start = video.samples.all()[0].decode_time
            + u64::try_from(video.samples.all()[0].composition_offset).unwrap();
        assert_eq!(
            video_start * 48_000,
            audio.samples.all()[0].decode_time * 15_360
        );
    }

    #[test]
    fn a_leading_empty_edit_delays_audio_by_exactly_that_much() {
        let index = parse_fixture("h264-aac-audio-delay.mp4");
        let video = track_of(&index, TrackKey::VIDEO);
        let audio = track_of(&index, TrackKey::audio(1));

        assert_eq!(video.timeline_shift, 0);
        assert_eq!(audio.timeline_shift, 26_176);
        // The gap between the tracks is the empty edit: 22976 ticks of 48000.
        let video_start = video.samples.all()[0].decode_time
            + u64::try_from(video.samples.all()[0].composition_offset).unwrap();
        let gap = audio.samples.all()[0].decode_time * 15_360 - video_start * 48_000;
        assert_eq!(gap, 22_976 * 15_360);
    }

    #[test]
    fn a_file_without_edit_lists_keeps_its_timestamps() {
        let index = parse_fixture("h264-aac.mp4");

        assert!(index.tracks.iter().all(|track| track.timeline_shift == 0));
        assert_eq!(index.tracks[1].samples.all()[0].decode_time, 0);
        assert_eq!(index.tracks[1].samples.len(), 141 + 1);
    }

    /// Every fragmented fixture is a `-c copy` remux of the progressive fixture, so it must
    /// index to the same samples: the same sizes, durations, flags, and payload bytes, in the
    /// same places relative to one another.
    #[test]
    fn a_fragmented_file_indexes_like_the_progressive_file_it_was_made_from() {
        let progressive = parse_fixture("h264-aac.mp4");
        let progressive_bytes = std::fs::read(fixture("h264-aac.mp4")).unwrap();
        for name in FRAGMENTED_FIXTURES {
            let fragmented = parse_fixture(name);
            let fragmented_bytes = std::fs::read(fixture(name)).unwrap();
            assert_eq!(fragmented.tracks.len(), progressive.tracks.len(), "{name}");

            for (fragment, original) in fragmented.tracks.iter().zip(&progressive.tracks) {
                let at = |i: usize| format!("{name} track {} sample {i}", fragment.id);
                assert_eq!(fragment.kind, original.kind, "{name}");
                assert_eq!(fragment.codec, original.codec, "{name}");
                assert_eq!(fragment.samples.len(), original.samples.len(), "{name}");
                for (i, (got, want)) in fragment.samples.iter().zip(&original.samples).enumerate() {
                    assert_eq!(got.size, want.size, "{}: size", at(i));
                    assert_eq!(got.duration, want.duration, "{}: duration", at(i));
                    assert_eq!(got.is_sync, want.is_sync, "{}: sync", at(i));
                    let range = |sample: &crate::media::Sample| {
                        let start = usize::try_from(sample.offset).unwrap();
                        start..start + usize::try_from(sample.size).unwrap()
                    };
                    assert_eq!(
                        fragmented_bytes[range(&got)],
                        progressive_bytes[range(&want)],
                        "{}: payload",
                        at(i)
                    );
                }
            }
        }
    }

    const FRAGMENTED_FIXTURES: [&str; 6] = [
        "h264-aac-fragmented.mp4",
        "h264-aac-fragmented-legacy.mp4",
        "h264-aac-fragmented-cmaf.mp4",
        "h264-aac-fragmented-sidx.mp4",
        "h264-aac-fragmented-offset.mp4",
        "h264-aac-fragmented-negative-cts.mp4",
    ];

    #[test]
    fn a_fragmented_file_has_the_same_timing_as_its_progressive_original() {
        let progressive = parse_fixture("h264-aac.mp4");
        // Some variants (`cmaf`, `negative_cts_offsets`) have FFmpeg shift every composition
        // offset by a constant to make them negative. That changes the offsets but not the
        // timing, so those are compared by how samples differ from the first, and the rest also
        // by absolute value.
        for name in FRAGMENTED_FIXTURES {
            let shifts_offsets = name.contains("negative") || name.contains("cmaf");
            let fragmented = parse_fixture(name);
            for (fragment, original) in fragmented.tracks.iter().zip(&progressive.tracks) {
                let relative = |track: &Track| {
                    let first = &track.samples.all()[0];
                    track
                        .samples
                        .all()
                        .iter()
                        .map(|s| {
                            (
                                s.decode_time - first.decode_time,
                                i64::from(s.composition_offset)
                                    - i64::from(first.composition_offset),
                            )
                        })
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    relative(fragment),
                    relative(original),
                    "{name} track {}",
                    fragment.id
                );
                if !shifts_offsets {
                    let absolute = |track: &Track| {
                        track
                            .samples
                            .all()
                            .iter()
                            .map(|s| (s.decode_time, s.composition_offset))
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(
                        absolute(fragment),
                        absolute(original),
                        "{name} track {}",
                        fragment.id
                    );
                }
                assert_eq!(
                    fragment.duration, original.duration,
                    "{name} track {}",
                    fragment.id
                );
            }
            // The movie timescales differ between files, so compare durations in seconds:
            // a/ts_a == b/ts_b is a·ts_b == b·ts_a.
            assert_eq!(
                u128::from(fragmented.duration) * u128::from(progressive.movie_timescale),
                u128::from(progressive.duration) * u128::from(fragmented.movie_timescale),
                "{name}: movie duration"
            );
        }
    }

    #[test]
    fn a_timeline_that_starts_at_100_seconds_is_moved_to_zero() {
        let plain = parse_fixture("h264-aac-fragmented.mp4");
        let offset = parse_fixture("h264-aac-fragmented-offset.mp4");

        // The two files differ only in their `tfdt` values, so once the origin is moved every
        // sample, including where its bytes are, must be the same.
        for (moved, original) in offset.tracks.iter().zip(&plain.tracks) {
            assert_eq!(moved.samples.all()[0].decode_time, 0, "track {}", moved.id);
            assert_eq!(moved.samples, original.samples, "track {}", moved.id);
            // The end is a duration, not a timestamp near 100 seconds.
            assert_eq!(moved.duration, original.duration, "track {}", moved.id);
        }
    }

    #[test]
    fn signed_composition_offsets_survive_version_one_trun_boxes() {
        let index = parse_fixture("h264-aac-fragmented-negative-cts.mp4");

        assert!(
            index.tracks[0]
                .samples
                .all()
                .iter()
                .any(|sample| sample.composition_offset < 0),
            "the fixture must carry negative offsets for this test to mean anything"
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
                for (position, sample) in track.samples.all().iter().enumerate() {
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

    /// A byte for a corruption: half the time an extreme, which stresses counts and sizes.
    fn extreme_or_random(next: &mut impl FnMut() -> u64) -> u8 {
        match next() % 4 {
            0 => 0xff,
            1 => 0x00,
            _ => u8::try_from(next() & 0xff).unwrap(),
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
            "h264-aac-fragmented.mp4",
            "h264-aac-fragmented-legacy.mp4",
            "h264-aac-fragmented-cmaf.mp4",
            "h264-aac-fragmented-negative-cts.mp4",
        ] {
            let source = open_kind(fixture(name)).expect("fixture should open");
            let original = block_on(Metadata::fetch(&source, &LimitsConfig::default()))
                .expect("fixture has moov");
            let identity = source.identity().clone();
            let len = original.len();
            for _ in 0..400 {
                let mut moov = original.moov_bytes().to_vec();
                let mut fragments = original.fragments().to_vec();
                for _ in 0..=(next() % 4) {
                    // In a fragmented file, half the corruption lands in a `moof`.
                    let target = if fragments.is_empty() || next() % 2 == 0 {
                        &mut moov
                    } else {
                        let which = usize::try_from(next()).unwrap() % fragments.len();
                        let mut bytes = fragments[which].bytes.to_vec();
                        let position = usize::try_from(next()).unwrap() % bytes.len();
                        bytes[position] = extreme_or_random(&mut next);
                        fragments[which].bytes = bytes::Bytes::from(bytes);
                        continue;
                    };
                    let position = usize::try_from(next()).unwrap() % target.len();
                    target[position] = extreme_or_random(&mut next);
                }
                let metadata = Metadata::from_parts(len, moov, fragments);
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
            .all()
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

    /// The video and audio protection of a protected fixture, with the codecs they were.
    fn protected_tracks(name: &str) -> (Track, Track) {
        let source = open_kind(fixture(name)).expect("fixture should open");
        let index = parse_index(&source, &LimitsConfig::default()).expect("fixture should parse");
        let mut tracks = index.tracks.into_iter();
        let video = tracks.next().expect("a video track");
        let audio = tracks.next().expect("an audio track");
        assert!(matches!(video.codec, CodecConfig::Avc { .. }), "{name}");
        assert!(matches!(audio.codec, CodecConfig::Aac { .. }), "{name}");
        (video, audio)
    }

    fn protection(track: &Track) -> &crate::media::SourceProtection {
        track
            .source_protection
            .as_deref()
            .expect("a protected track")
    }

    const FIXTURE_KEY_ID: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];

    /// The IV `tests/fixtures/protect-fixture.py` gives sample `index` of track `track`.
    fn fixture_iv(track: usize, index: usize, size: usize) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        Sha256::digest(format!("iv {track} {index}").as_bytes())[..size].to_vec()
    }

    /// `FFmpeg` picks fresh random IVs each time the fixture is generated, so this checks what
    /// `senc` must look like, not the bytes: the end-to-end test against the clear twin checks
    /// that the IVs are the right ones.
    #[test]
    fn a_real_encoders_cenc_file_is_read_with_its_ivs_and_subsamples() {
        let (video_track, audio_track) = protected_tracks("h264-aac-cenc-ffmpeg.mp4");
        let (video, audio) = (protection(&video_track), protection(&audio_track));
        assert_eq!(video.scheme, crate::media::SourceScheme::Cenc);
        assert_eq!(video.original_format, *b"avc1");
        assert_eq!(audio.original_format, *b"mp4a");
        assert_eq!(video.key_id, FIXTURE_KEY_ID);
        assert_eq!(video.constant_iv, None);
        for (track, protection) in [(&video_track, video), (&audio_track, audio)] {
            let count = track.samples.len();
            assert_eq!(protection.aux.iv(0).map(<[u8]>::len), Some(8));
            assert_eq!(protection.aux.iv(count - 1).map(<[u8]>::len), Some(8));
            assert!(protection.aux.iv(count).is_none());
        }
        // Video is protected NAL by NAL, each after a clear length prefix and NAL header;
        // audio is protected whole, so has no subsample map. The `roll` group is not `seig`.
        for index in 0..video_track.samples.len() {
            let subsamples = video.aux.subsamples(index);
            assert!(!subsamples.is_empty(), "video sample {index}");
            assert!(subsamples.iter().all(|s| s.clear == 5 && s.protected > 0));
            let total: u64 = subsamples
                .iter()
                .map(|s| u64::from(s.clear) + u64::from(s.protected))
                .sum();
            assert_eq!(
                total,
                u64::from(video_track.samples.size(index)),
                "video sample {index}"
            );
        }
        assert_eq!(audio.aux.subsamples(0), []);
    }

    #[test]
    fn each_scheme_is_read_with_its_pattern_and_iv_layout() {
        // (fixture, scheme, IV size, video pattern, audio pattern)
        let cases = [
            (
                "h264-aac-cenc.mp4",
                crate::media::SourceScheme::Cenc,
                8,
                (0, 0),
                (0, 0),
            ),
            (
                "h264-aac-cens.mp4",
                crate::media::SourceScheme::Cens,
                8,
                (1, 9),
                (1, 9),
            ),
            (
                "h264-aac-cbc1.mp4",
                crate::media::SourceScheme::Cbc1,
                16,
                (0, 0),
                (0, 0),
            ),
        ];
        for (name, scheme, iv_size, video_pattern, audio_pattern) in cases {
            let (video_track, audio_track) = protected_tracks(name);
            let (video, audio) = (protection(&video_track), protection(&audio_track));
            assert_eq!(video.scheme, scheme, "{name}");
            assert_eq!(audio.scheme, scheme, "{name}");
            assert_eq!(video.key_id, FIXTURE_KEY_ID, "{name}");
            assert_eq!(
                (video.crypt_blocks, video.skip_blocks),
                video_pattern,
                "{name}"
            );
            assert_eq!(
                (audio.crypt_blocks, audio.skip_blocks),
                audio_pattern,
                "{name}"
            );
            // Sample 7 of each track has the IV the generator derived for it.
            assert_eq!(
                video.aux.iv(7),
                Some(&fixture_iv(0, 7, iv_size)[..]),
                "{name}"
            );
            assert_eq!(
                audio.aux.iv(7),
                Some(&fixture_iv(1, 7, iv_size)[..]),
                "{name}"
            );
            let subsamples = video.aux.subsamples(0);
            assert!(
                !subsamples.is_empty(),
                "{name}: video is protected NAL by NAL"
            );
            assert!(
                audio.aux.subsamples(0).is_empty(),
                "{name}: audio is protected whole"
            );
            if scheme == crate::media::SourceScheme::Cbc1 {
                assert!(
                    subsamples
                        .iter()
                        .all(|subsample| subsample.protected % 16 == 0),
                    "{name}: cbc1 protects whole blocks"
                );
            }
        }
    }

    /// A fragmented file keeps each sample's IV in the `senc` of its own `traf`: the samples of
    /// every fragment, in order, are the ones the IVs were derived for.
    #[test]
    fn every_sample_of_every_fragment_has_the_iv_made_for_it() {
        let cases = [
            (
                "h264-aac-fragmented-cmaf-cenc.mp4",
                crate::media::SourceScheme::Cenc,
                8,
            ),
            (
                "h264-aac-fragmented-cmaf-cens.mp4",
                crate::media::SourceScheme::Cens,
                8,
            ),
            (
                "h264-aac-fragmented-cmaf-cbc1.mp4",
                crate::media::SourceScheme::Cbc1,
                16,
            ),
        ];
        for (name, scheme, iv_size) in cases {
            let (video_track, audio_track) = protected_tracks(name);
            for (number, track) in [&video_track, &audio_track].into_iter().enumerate() {
                let protection = protection(track);
                assert_eq!(protection.scheme, scheme, "{name}");
                let count = track.samples.len();
                assert!(count > 40, "{name}: the samples of several fragments");
                for index in 0..count {
                    assert_eq!(
                        protection.aux.iv(index),
                        Some(&fixture_iv(number, index, iv_size)[..]),
                        "{name} track {number} sample {index}"
                    );
                }
                assert!(protection.aux.iv(count).is_none(), "{name}");
            }
            // Video is protected NAL by NAL and each map covers its sample exactly.
            let video = protection(&video_track);
            for index in 0..video_track.samples.len() {
                let total: u64 = video
                    .aux
                    .subsamples(index)
                    .iter()
                    .map(|s| u64::from(s.clear) + u64::from(s.protected))
                    .sum();
                assert_eq!(
                    total,
                    u64::from(video_track.samples.size(index)),
                    "{name} {index}"
                );
            }
        }
        let (video, audio) = protected_tracks("h264-aac-fragmented-cmaf-cbcs.mp4");
        for track in [&video, &audio] {
            assert_eq!(protection(track).constant_iv.map(|iv| iv[0]), Some(0xc0));
            assert!(protection(track).aux.iv(0).is_none());
        }
        assert_ne!(protection(&video).aux.subsamples(0), []);
        assert_eq!(protection(&audio).aux.bytes(), 0, "audio needs no senc");
    }

    #[test]
    fn cbcs_has_a_constant_iv_and_no_per_sample_ivs() {
        let (video_track, audio_track) = protected_tracks("h264-aac-cbcs.mp4");
        let (video, audio) = (protection(&video_track), protection(&audio_track));
        assert_eq!(video.scheme, crate::media::SourceScheme::Cbcs);
        let constant = [
            0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd,
            0xce, 0xcf,
        ];
        assert_eq!(video.constant_iv, Some(constant));
        assert_eq!(audio.constant_iv, Some(constant));
        assert_eq!((video.crypt_blocks, video.skip_blocks), (1, 9));
        assert_eq!((audio.crypt_blocks, audio.skip_blocks), (0, 0));
        assert!(video.aux.iv(0).is_none());
        assert_ne!(video.aux.subsamples(0), []);
        // Audio has no `senc` at all: a constant IV and whole-sample protection need none.
        assert_eq!(audio.aux.bytes(), 0);
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
        let mut mutated = original;
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

    /// TDD 0010: the compact index holds exactly the samples the one-record-per-sample
    /// expansion produced, for every track of every progressive fixture.
    #[test]
    fn the_compact_index_matches_the_expansion_it_replaced_on_every_fixture() {
        let limits = LimitsConfig::default();
        let mut tracks = 0;
        for entry in std::fs::read_dir(fixture("")).unwrap() {
            let path = entry.unwrap().path();
            if !matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("mp4" | "m4a" | "mov")
            ) {
                continue;
            }
            let source = open_kind(path.clone()).unwrap();
            let metadata = block_on(Metadata::fetch(&source, &limits)).unwrap();
            let moov = validate_raw_moov(metadata.moov_bytes()).unwrap();
            if moov.fragmented {
                continue;
            }
            for raw in &moov.tracks {
                if matches!(raw.disposition, Disposition::Skip(_)) {
                    continue;
                }
                let media = required_child(raw.trak, *b"mdia").unwrap();
                let minf = required_child(media.payload, *b"minf").unwrap();
                let stbl = required_child(minf.payload, *b"stbl").unwrap();
                let tables = parse_sample_tables(stbl.payload).unwrap();
                let expected =
                    super::super::tables::expand_samples(&tables, source.len(), &limits).unwrap();
                let index = sample_index(tables, source.len(), &limits).unwrap();
                assert_eq!(index.all(), expected, "{} track {}", path.display(), raw.id);
                tracks += 1;
            }
        }
        assert!(tracks > 40, "only {tracks} tracks compared");
    }

    fn fixture_moov() -> Vec<u8> {
        let source = open_kind(fixture("h264-aac.mp4")).expect("fixture should open");
        block_on(Metadata::fetch(&source, &LimitsConfig::default()))
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
