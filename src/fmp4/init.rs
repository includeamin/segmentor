//! Initialization segments, built from the source's own boxes.
//!
//! The segment is assembled at the byte level so that what a player needs to render the track
//! (aspect ratio, colour, HDR, codec configuration) is copied, not re-created: the sample entry
//! (`stsd`) is passed through untouched, and only the parts that describe *where the samples
//! are* are replaced with empty tables, as fragmented MP4 requires.

use crate::error::{Error, Result};
use crate::mp4::boxes::{RawBox, Reader, box_payload, child_boxes, required_child};
use crate::mp4::codec::iso_audio_entry;
use crate::source::Metadata;

/// Major brand `iso6` (fragmented MP4 with default-base-is-moof), with `iso6` and `mp41`
/// compatible. The source's own brands describe the source file, not this stream.
const FTYP: [u8; 24] = *b"\x00\x00\x00\x18ftypiso6\x00\x00\x00\x00iso6mp41";

pub(crate) fn write_init_segment(metadata: &Metadata, track_id: u32) -> Result<Vec<u8>> {
    let moov = box_payload(metadata.moov_bytes(), 0)?;
    let children = child_boxes(moov.payload)?;
    let movie_header = children
        .iter()
        .find(|child| child.name == *b"mvhd")
        .ok_or_else(|| Error::InvalidMedia("required MP4 box `mvhd` is missing".to_owned()))?;
    let track = children
        .iter()
        .filter(|child| child.name == *b"trak")
        .find(|track| track_id_of(track).is_ok_and(|id| id == track_id))
        .ok_or_else(|| Error::InvalidMedia(format!("track {track_id} does not exist")))?;

    let mut movie = Vec::new();
    write_box(
        &mut movie,
        *b"mvhd",
        &with_zero_duration(movie_header.payload, DurationAt::MOVIE_OR_MEDIA)?,
    )?;
    write_box(&mut movie, *b"trak", &track_box(track, track_id)?)?;
    write_box(&mut movie, *b"mvex", &track_extends(track_id))?;

    let mut output = FTYP.to_vec();
    write_box(&mut output, *b"moov", &movie)?;
    Ok(output)
}

fn track_id_of(track: &RawBox<'_>) -> Result<u32> {
    let mut reader = Reader::new(required_child(track.payload, *b"tkhd")?.payload);
    let version = reader.full_box()?;
    reader.skip(if version == 1 { 16 } else { 8 })?;
    reader.u32()
}

/// Rebuilds a `trak` with the same boxes but no sample locations, edit list, or user data.
fn track_box(track: &RawBox<'_>, track_id: u32) -> Result<Vec<u8>> {
    let header = required_child(track.payload, *b"tkhd")?;
    let media = required_child(track.payload, *b"mdia")?;
    let mut output = Vec::new();
    write_box(
        &mut output,
        *b"tkhd",
        &with_zero_duration(header.payload, DurationAt::TRACK_HEADER)?,
    )?;
    // The edit list is not copied: it was applied to the sample timestamps when the file was
    // parsed, and a player would otherwise apply it a second time.
    write_box(&mut output, *b"mdia", &media_box(&media, track_id)?)?;
    Ok(output)
}

fn media_box(media: &RawBox<'_>, track_id: u32) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for child in child_boxes(media.payload)? {
        match &child.name {
            b"mdhd" => write_box(
                &mut output,
                *b"mdhd",
                &with_zero_duration(child.payload, DurationAt::MOVIE_OR_MEDIA)?,
            )?,
            b"minf" => write_box(&mut output, *b"minf", &media_info_box(&child, track_id)?)?,
            _ => write_box(&mut output, child.name, child.payload)?,
        }
    }
    Ok(output)
}

fn media_info_box(media_info: &RawBox<'_>, track_id: u32) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for child in child_boxes(media_info.payload)? {
        if child.name == *b"stbl" {
            write_box(&mut output, *b"stbl", &sample_table_box(&child, track_id)?)?;
        } else {
            // `vmhd`/`smhd` and `dinf`, exactly as they were.
            write_box(&mut output, child.name, child.payload)?;
        }
    }
    Ok(output)
}

/// The sample description, followed by the empty tables a fragmented file requires.
fn sample_table_box(sample_table: &RawBox<'_>, track_id: u32) -> Result<Vec<u8>> {
    let description = required_child(sample_table.payload, *b"stsd")?;
    let mut output = Vec::new();
    write_box(
        &mut output,
        *b"stsd",
        &sample_description(description.payload, track_id)?,
    )?;
    // `stts`, `stsc`, and `stco` are a version/flags word and an entry count of zero; `stsz`
    // adds a sample size before its count.
    write_box(&mut output, *b"stts", &[0; 8])?;
    write_box(&mut output, *b"stsc", &[0; 8])?;
    write_box(&mut output, *b"stsz", &[0; 12])?;
    write_box(&mut output, *b"stco", &[0; 8])?;
    Ok(output)
}

/// The `trex` box that goes inside `mvex`: version/flags, the track, default sample description
/// index 1, and zero defaults for duration, size, and flags (every sample says its own).
/// The `stsd` payload to write: the source's, verbatim, except for a QuickTime-style audio entry,
/// which browsers refuse and which is rewritten in the standard layout.
fn sample_description(description: &[u8], track_id: u32) -> Result<Vec<u8>> {
    let mut reader = Reader::new(description);
    reader.full_box()?;
    reader.skip(4)?;
    let entry = child_boxes(reader.rest())?
        .into_iter()
        .next()
        .ok_or_else(|| Error::InvalidMedia("stsd entry is missing".to_owned()))?;
    if entry.name != *b"mp4a" {
        return Ok(description.to_vec());
    }
    let Some(iso) = iso_audio_entry(entry.payload, track_id)? else {
        return Ok(description.to_vec());
    };
    // The version, flags, and entry count of the source's `stsd`, then the rewritten entry.
    let mut rewritten = description[..8].to_vec();
    write_box(&mut rewritten, *b"mp4a", &iso)?;
    Ok(rewritten)
}

fn track_extends(track_id: u32) -> Vec<u8> {
    let mut payload = vec![0; 4];
    payload.extend_from_slice(&track_id.to_be_bytes());
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.extend_from_slice(&[0; 12]);
    let mut trex = Vec::with_capacity(32);
    trex.extend_from_slice(&32u32.to_be_bytes());
    trex.extend_from_slice(b"trex");
    trex.extend_from_slice(&payload);
    trex
}

/// Where a header box keeps its duration, as an offset into the payload for each version. The
/// field is four bytes in version 0 and eight in version 1.
#[derive(Clone, Copy)]
struct DurationAt {
    version_0: usize,
    version_1: usize,
}

impl DurationAt {
    /// `mvhd` and `mdhd` share a layout: version/flags, two times, timescale, duration.
    const MOVIE_OR_MEDIA: Self = Self {
        version_0: 16,
        version_1: 24,
    };
    /// `tkhd`: version/flags, two times, track ID, reserved, duration.
    const TRACK_HEADER: Self = Self {
        version_0: 20,
        version_1: 28,
    };
}

/// A copy of a header payload with its duration set to zero, as a fragmented init requires.
fn with_zero_duration(payload: &[u8], at: DurationAt) -> Result<Vec<u8>> {
    let (offset, width) = match payload.first() {
        Some(1) => (at.version_1, 8),
        Some(_) => (at.version_0, 4),
        None => return Err(Error::InvalidMedia("header box is empty".to_owned())),
    };
    let mut copy = payload.to_vec();
    copy.get_mut(offset..offset + width)
        .ok_or_else(|| Error::InvalidMedia("header box is truncated".to_owned()))?
        .fill(0);
    Ok(copy)
}

fn write_box(output: &mut Vec<u8>, name: [u8; 4], payload: &[u8]) -> Result<()> {
    let size = payload
        .len()
        .checked_add(8)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| Error::InvalidMedia("initialization segment is too large".to_owned()))?;
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(&name);
    output.extend_from_slice(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::LimitsConfig;
    use crate::media::TrackKey;
    use crate::mp4;
    use crate::source::{LocalMediaSource, MediaSourceKind};

    fn parsed(name: &str) -> mp4::ParsedMedia {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let source = MediaSourceKind::Local(std::sync::Arc::new(
            LocalMediaSource::open(path).expect("fixture should open"),
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build")
            .block_on(mp4::parse(&source, &LimitsConfig::default()))
            .expect("fixture should parse")
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn the_init_segment_does_not_repeat_the_edit_list_the_timestamps_already_apply() {
        let media = parsed("h264-aac-default-edits.mp4");
        assert!(
            contains(media.metadata.moov_bytes(), b"elst"),
            "the fixture must have an edit list for this test to mean anything"
        );

        for track in &media.index.tracks {
            let init = write_init_segment(&media.metadata, track.id).unwrap();
            assert!(
                !contains(&init, b"edts") && !contains(&init, b"elst"),
                "{} init still carries an edit list",
                track.key
            );
        }
        assert_eq!(media.index.tracks[0].key, TrackKey::VIDEO);
    }

    /// The payload of the box at `path` below the top level of `bytes`.
    fn at<'a>(bytes: &'a [u8], path: &[&[u8; 4]]) -> &'a [u8] {
        let mut current = bytes;
        for name in path {
            current = required_child(current, **name)
                .unwrap_or_else(|error| panic!("{path:?}: {error}"))
                .payload;
        }
        current
    }

    fn init_of(name: &str, track: TrackKey) -> (Vec<u8>, mp4::ParsedMedia) {
        let media = parsed(name);
        let id = media
            .index
            .tracks
            .iter()
            .find(|t| t.key == track)
            .unwrap()
            .id;
        (write_init_segment(&media.metadata, id).unwrap(), media)
    }

    #[test]
    fn the_sample_entry_is_copied_byte_for_byte_so_aspect_ratio_and_colour_survive() {
        let (init, media) = init_of("h264-aac-anamorphic.mp4", TrackKey::VIDEO);

        let source_moov = box_payload(media.metadata.moov_bytes(), 0).unwrap().payload;
        let source_trak = child_boxes(source_moov)
            .unwrap()
            .into_iter()
            .find(|child| child.name == *b"trak")
            .unwrap();
        let path = [b"mdia", b"minf", b"stbl", b"stsd"];
        let source_stsd = at(source_trak.payload, &path);
        let init_stsd = at(
            &init[FTYP.len() + 8..],
            &[b"trak", b"mdia", b"minf", b"stbl", b"stsd"],
        );

        assert_eq!(init_stsd, source_stsd, "stsd must be identical");
        assert!(contains(init_stsd, b"pasp"), "pixel aspect ratio is kept");
        assert!(contains(init_stsd, b"colr"), "colour information is kept");
        assert!(contains(init_stsd, b"avcC"), "codec configuration is kept");
    }

    #[test]
    fn starts_with_a_fixed_ftyp_whatever_the_source_brand_was() {
        for name in ["h264-aac.mp4", "h264-aac-quicktime.mov"] {
            let (init, _) = init_of(name, TrackKey::VIDEO);

            assert_eq!(&init[..FTYP.len()], FTYP, "{name}");
            assert_eq!(&init[4..12], b"ftypiso6", "{name}");
            assert_eq!(&init[FTYP.len() + 4..FTYP.len() + 8], b"moov", "{name}");
        }
    }

    #[test]
    fn holds_exactly_the_requested_track_with_empty_tables_and_zero_durations() {
        let (video, media) = init_of("h264-aac.mp4", TrackKey::VIDEO);
        let (audio, _) = init_of("h264-aac.mp4", TrackKey::audio(1));

        assert!(contains(&video, b"vide") && !contains(&video, b"soun"));
        assert!(contains(&audio, b"soun") && !contains(&audio, b"vide"));
        let moov = &video[FTYP.len() + 8..];
        assert_eq!(
            child_boxes(moov)
                .unwrap()
                .iter()
                .filter(|c| c.name == *b"trak")
                .count(),
            1
        );
        let track = media.index.tracks[0].id;
        let trex = at(moov, &[b"mvex", b"trex"]);
        assert_eq!(&trex[4..8], &track.to_be_bytes(), "trex names the track");

        // Movie duration is bytes 16..20 of a version 0 mvhd; media duration likewise in mdhd.
        assert_eq!(&at(moov, &[b"mvhd"])[16..20], [0; 4]);
        assert_eq!(&at(moov, &[b"trak", b"mdia", b"mdhd"])[16..20], [0; 4]);
        let stbl = [b"trak", b"mdia", b"minf", b"stbl"];
        for table in [b"stts", b"stsc", b"stco"] {
            let path = [&stbl[..], &[table]].concat();
            assert_eq!(at(moov, &path), [0; 8], "{table:?} is empty");
        }
        let path = [&stbl[..], &[b"stsz"]].concat();
        assert_eq!(at(moov, &path), [0; 12], "stsz is empty");
    }

    #[test]
    fn a_missing_track_is_an_error() {
        let media = parsed("h264-aac.mp4");

        assert!(write_init_segment(&media.metadata, 99).is_err());
    }

    /// The `mp4a` entry payload of the audio track's `stsd`, in a moov or an init segment.
    fn audio_entry(moov: &[u8], audio_track: usize) -> Vec<u8> {
        let track = child_boxes(moov)
            .unwrap()
            .into_iter()
            .filter(|child| child.name == *b"trak")
            .nth(audio_track)
            .unwrap();
        let stsd = at(track.payload, &[b"mdia", b"minf", b"stbl", b"stsd"]);
        child_boxes(&stsd[8..]).unwrap()[0].payload.to_vec()
    }

    #[test]
    fn a_quicktime_audio_entry_is_rewritten_in_the_standard_layout() {
        let (init, media) = init_of("h264-aac-quicktime.mov", TrackKey::audio(1));
        let source_moov = box_payload(media.metadata.moov_bytes(), 0).unwrap().payload;
        let source = audio_entry(source_moov, 1);
        assert_eq!(
            &source[8..10],
            [0, 1],
            "the fixture must use sound version 1"
        );
        assert!(contains(&source, b"wave"));

        let rewritten = audio_entry(&init[FTYP.len() + 8..], 0);

        assert_eq!(&rewritten[8..10], [0, 0], "version 0");
        assert!(contains(&rewritten, b"esds"));
        assert!(!contains(&rewritten, b"wave") && !contains(&rewritten, b"chan"));
        // Same audio, described the standard way.
        assert_eq!(
            crate::mp4::codec::parse_aac(&rewritten, 2).unwrap(),
            crate::mp4::codec::parse_aac(&source, 2).unwrap()
        );
        assert_eq!(
            &rewritten[..8],
            &source[..8],
            "data reference index is kept"
        );
        assert_eq!(&rewritten[16..18], &source[16..18], "channel count is kept");
        assert_eq!(&rewritten[24..28], &source[24..28], "sample rate is kept");
    }

    #[test]
    fn a_standard_audio_entry_is_left_exactly_as_it_was() {
        let (init, media) = init_of("h264-aac.mp4", TrackKey::audio(1));
        let source_moov = box_payload(media.metadata.moov_bytes(), 0).unwrap().payload;

        assert_eq!(
            audio_entry(&init[FTYP.len() + 8..], 0),
            audio_entry(source_moov, 1)
        );
    }
}
