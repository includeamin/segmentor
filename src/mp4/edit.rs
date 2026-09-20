//! Edit lists: mapping every track's media timeline onto one shared presentation timeline.
//!
//! Encoders write an edit list to hide encoder delay (AAC priming) and B-frame reordering
//! delay, and to offset one track from another. Fragmented MP4 cannot express a decode time
//! before zero, so the edit is applied by shifting every track forward by one shared offset
//! instead of subtracting `media_time`. See TDD 0004 for the derivation.
//!
//! A track's edit list is one non-empty edit at rate 1, optionally preceded by one empty edit
//! that delays the track's start. Anything else is rejected rather than approximated.

use crate::error::{Error, Result};
use crate::media::{Sample, Track, TrackKind};

/// The most `elst` entries read before the shape is checked, so a hostile file cannot make the
/// check itself expensive.
const MAX_EDIT_ENTRIES: usize = 16;

/// One `elst` entry, as read from the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ElstEntry {
    /// In movie ticks.
    pub(super) segment_duration: u64,
    /// In track ticks; all ones marks an empty edit.
    pub(super) media_time: u64,
    pub(super) media_rate: u16,
    pub(super) media_rate_fraction: u16,
}

/// What one track's edit list asks for, in the units named on each field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TrackEdit {
    /// Media time at which presentation starts, in track ticks.
    pub(super) media_time: u64,
    /// Presentation time that passes before the media starts, in movie ticks.
    pub(super) delay: u64,
    /// Media time at which the edit ends, in track ticks. `None` plays to the end.
    pub(super) media_end: Option<u64>,
}

impl TrackEdit {
    /// The edit of a track with no edit list.
    pub(super) const NONE: Self = Self {
        media_time: 0,
        delay: 0,
        media_end: None,
    };
}

/// Reads one track's edit list into a [`TrackEdit`], or explains why it cannot be honored.
pub(super) fn parse_edit_list(
    track_id: u32,
    version: u8,
    entries: &[ElstEntry],
    movie_timescale: u32,
    track_timescale: u32,
) -> Result<TrackEdit> {
    if entries.len() > MAX_EDIT_ENTRIES {
        return Err(unsupported(
            track_id,
            format!("edit list has {} entries", entries.len()),
        ));
    }
    let (delay, edit) = match entries {
        [] => return Ok(TrackEdit::NONE),
        [edit] if !is_empty_edit(edit, version) => (0, edit),
        [gap, edit] if is_empty_edit(gap, version) && !is_empty_edit(edit, version) => {
            (gap.segment_duration, edit)
        }
        _ => {
            return Err(unsupported(
                track_id,
                format!(
                    "edit list shape is not supported ({} entries; only one edit, optionally \
                     after one empty edit, is)",
                    entries.len()
                ),
            ));
        }
    };
    if edit.media_rate != 1 || edit.media_rate_fraction != 0 {
        return Err(unsupported(
            track_id,
            format!(
                "edit list plays media at rate {}.{}, only rate 1 is supported",
                edit.media_rate, edit.media_rate_fraction
            ),
        ));
    }
    let negative_limit = if version == 1 {
        i64::MAX as u64
    } else {
        i32::MAX as u64
    };
    if edit.media_time > negative_limit {
        return Err(Error::InvalidMedia(format!(
            "track {track_id}: edit list media_time is negative"
        )));
    }
    if movie_timescale == 0 || track_timescale == 0 {
        return Err(Error::InvalidMedia(format!(
            "track {track_id}: timescale is zero"
        )));
    }
    // A duration of zero means "to the end", which some writers use for the last edit.
    let media_end = (edit.segment_duration != 0)
        .then(|| {
            u128::from(edit.segment_duration) * u128::from(track_timescale)
                / u128::from(movie_timescale)
        })
        .map(|end| u128::from(edit.media_time) + end)
        .map(|end| u64::try_from(end).unwrap_or(u64::MAX));
    Ok(TrackEdit {
        media_time: edit.media_time,
        delay,
        media_end,
    })
}

/// `media_time` of -1 marks an empty edit: the presentation waits without playing media.
fn is_empty_edit(entry: &ElstEntry, version: u8) -> bool {
    if version == 1 {
        entry.media_time == u64::MAX
    } else {
        entry.media_time == u64::from(u32::MAX)
    }
}

fn unsupported(track_id: u32, detail: impl std::fmt::Display) -> Error {
    Error::Unsupported(format!("track {track_id}: {detail}"))
}

/// One shift per input, in that track's ticks, from the shared offset `O`:
///
/// ```text
/// O       = max(0, max over tracks of (media_time / timescale − delay / movie_timescale))
/// shift_t = round((delay_t / movie_timescale + O) · timescale_t) − media_time_t
/// ```
///
/// Every shift is at least zero, and one track's is exactly zero whenever `O` is positive.
pub(super) fn timeline_shifts(
    edits: &[(TrackEdit, u32)],
    movie_timescale: u32,
) -> Result<Vec<u64>> {
    let movie = i128::from(movie_timescale);
    // O as a fraction: (numerator, denominator), denominator positive.
    let mut offset = (0i128, 1i128);
    for (edit, timescale) in edits {
        let timescale = i128::from(*timescale);
        let numerator = i128::from(edit.media_time) * movie - i128::from(edit.delay) * timescale;
        let denominator = timescale * movie;
        // numerator/denominator > offset.0/offset.1, without dividing.
        if numerator * offset.1 > offset.0 * denominator {
            offset = (numerator, denominator);
        }
    }
    edits
        .iter()
        .map(|(edit, timescale)| {
            let timescale = i128::from(*timescale);
            let scaled = (i128::from(edit.delay) * offset.1 + offset.0 * movie) * timescale;
            let denominator = movie * offset.1;
            // Round half up.
            let ticks = (2 * scaled + denominator).div_euclid(2 * denominator);
            u64::try_from(ticks - i128::from(edit.media_time)).map_err(|_| {
                Error::InvalidMedia("edit list timeline shift is out of range".to_owned())
            })
        })
        .collect()
}

/// Trims `track` to its edit and moves it onto the shared timeline.
///
/// Does nothing for a track with no edit list and no shift, so untouched files keep the exact
/// timestamps and duration they were parsed with.
pub(super) fn apply(track: &mut Track, edit: TrackEdit, shift: u64) -> Result<()> {
    if edit == TrackEdit::NONE && shift == 0 {
        return Ok(());
    }
    if let Some(end) = edit.media_end {
        trim_tail(track, end)?;
    }
    if track.kind == TrackKind::Audio {
        // Whole leading samples that end before the edit starts are encoder padding.
        let keep_from = track
            .samples
            .partition_point(|sample| sample_end(sample) <= edit.media_time);
        track.samples.drain(..keep_from);
    }
    let last = track
        .samples
        .last()
        .ok_or_else(|| {
            Error::InvalidMedia(format!(
                "track {}: edit list removes every sample",
                track.id
            ))
        })?
        .to_owned();
    for sample in &mut track.samples {
        sample.decode_time = sample
            .decode_time
            .checked_add(shift)
            .ok_or_else(|| Error::InvalidMedia("decode timestamp overflow".to_owned()))?;
    }
    track.timeline_shift = shift;
    track.duration = sample_end(&last)
        .checked_add(shift)
        .ok_or_else(|| Error::InvalidMedia("track duration overflow".to_owned()))?;
    Ok(())
}

fn sample_end(sample: &Sample) -> u64 {
    sample
        .decode_time
        .saturating_add(u64::from(sample.duration))
}

/// Drops the samples that present at or after `end`. They must form a suffix in decode order,
/// which is what a plain end trim looks like; anything else would need reordering.
fn trim_tail(track: &mut Track, end: u64) -> Result<()> {
    let presents_at =
        |sample: &Sample| i128::from(sample.decode_time) + i128::from(sample.composition_offset);
    let end = i128::from(end);
    let Some(first_cut) = track
        .samples
        .iter()
        .position(|sample| presents_at(sample) >= end)
    else {
        return Ok(());
    };
    if track.samples[first_cut..]
        .iter()
        .any(|sample| presents_at(sample) < end)
    {
        return Err(unsupported(
            track.id,
            "edit list ends inside a run of reordered samples".to_owned(),
        ));
    }
    if first_cut == 0 {
        return Err(Error::InvalidMedia(format!(
            "track {}: edit list ends before the first sample",
            track.id
        )));
    }
    track.samples.truncate(first_cut);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{CodecConfig, TrackKey};

    fn entry(segment_duration: u64, media_time: u64) -> ElstEntry {
        ElstEntry {
            segment_duration,
            media_time,
            media_rate: 1,
            media_rate_fraction: 0,
        }
    }

    const EMPTY_V0: u64 = u32::MAX as u64;

    #[test]
    fn no_entries_means_no_edit() {
        assert_eq!(
            parse_edit_list(1, 0, &[], 1000, 48000).unwrap(),
            TrackEdit::NONE
        );
    }

    #[test]
    fn reads_the_ffmpeg_default_edit() {
        // 3 s in a 48 kHz movie timescale, starting 1024 ticks into a 15360 Hz track.
        let edit = parse_edit_list(1, 0, &[entry(144_000, 1024)], 48_000, 15_360).unwrap();

        assert_eq!(edit.media_time, 1024);
        assert_eq!(edit.delay, 0);
        assert_eq!(edit.media_end, Some(1024 + 46_080));
    }

    #[test]
    fn reads_a_leading_empty_edit() {
        let edit =
            parse_edit_list(1, 0, &[entry(500, EMPTY_V0), entry(2500, 0)], 1000, 48_000).unwrap();

        assert_eq!(edit.delay, 500);
        assert_eq!(edit.media_time, 0);
    }

    #[test]
    fn version_one_marks_empty_edits_with_the_all_ones_value() {
        let edit = parse_edit_list(1, 1, &[entry(500, u64::MAX), entry(0, 7)], 1000, 1000).unwrap();

        assert_eq!(edit.delay, 500);
        assert_eq!(edit.media_end, None, "zero duration plays to the end");
    }

    #[test]
    fn rejects_shapes_that_cut_repeat_or_change_speed() {
        let two_cuts = [entry(100, 0), entry(100, 500)];
        let mut fast = entry(100, 0);
        fast.media_rate = 2;
        let mut dwell = entry(100, 0);
        dwell.media_rate = 0;
        let mut fraction = entry(100, 0);
        fraction.media_rate_fraction = 1;

        for (name, entries) in [
            ("two cuts", &two_cuts[..]),
            ("double speed", &[fast][..]),
            ("dwell", &[dwell][..]),
            ("fractional rate", &[fraction][..]),
            ("only an empty edit", &[entry(100, EMPTY_V0)][..]),
        ] {
            let error = parse_edit_list(3, 0, entries, 1000, 1000).expect_err(name);
            assert!(matches!(error, Error::Unsupported(_)), "{name}: {error}");
            assert!(error.to_string().contains("track 3"), "{name}: {error}");
        }
    }

    #[test]
    fn rejects_a_negative_media_time_other_than_the_empty_marker() {
        let error = parse_edit_list(1, 0, &[entry(100, 0xFFFF_FFF0)], 1000, 1000)
            .expect_err("negative media time");

        assert!(matches!(error, Error::InvalidMedia(_)), "{error}");
    }

    #[test]
    fn rejects_an_absurd_number_of_entries() {
        let entries = vec![entry(1, 0); MAX_EDIT_ENTRIES + 1];

        assert!(parse_edit_list(1, 0, &entries, 1000, 1000).is_err());
    }

    #[test]
    fn ffmpeg_default_keeps_the_video_in_place_and_delays_audio() {
        // Video: 1024 ticks of 15360 (66.67 ms) of B-frame delay. Audio: 1024 ticks of 48000
        // (21.33 ms) of priming.
        let edits = [
            (
                TrackEdit {
                    media_time: 1024,
                    delay: 0,
                    media_end: None,
                },
                15_360,
            ),
            (
                TrackEdit {
                    media_time: 1024,
                    delay: 0,
                    media_end: None,
                },
                48_000,
            ),
        ];

        let shifts = timeline_shifts(&edits, 48_000).unwrap();

        assert_eq!(shifts, [0, 3200 - 1024]);
    }

    #[test]
    fn shifts_preserve_the_relative_offset_between_tracks() {
        // Video starts 100 ms into the presentation, audio at 0, with no media_time.
        let edits = [
            (
                TrackEdit {
                    media_time: 0,
                    delay: 100,
                    media_end: None,
                },
                90_000,
            ),
            (TrackEdit::NONE, 48_000),
        ];

        let shifts = timeline_shifts(&edits, 1000).unwrap();

        // Nothing needs to move: the delay is already positive.
        assert_eq!(shifts, [9000, 0]);
    }

    #[test]
    fn no_edit_lists_need_no_shift() {
        let edits = [(TrackEdit::NONE, 90_000), (TrackEdit::NONE, 48_000)];

        assert_eq!(timeline_shifts(&edits, 1000).unwrap(), [0, 0]);
    }

    #[test]
    fn every_shift_is_non_negative_and_sync_is_exact_for_uneven_timescales() {
        // media_time in different timescales that do not divide each other.
        for (m_a, ts_a, m_b, ts_b) in [
            (1001, 30_000, 1024, 44_100),
            (3, 7, 5, 11),
            (0, 90_000, 2112, 48_000),
        ] {
            let edits = [
                (
                    TrackEdit {
                        media_time: m_a,
                        delay: 0,
                        media_end: None,
                    },
                    ts_a,
                ),
                (
                    TrackEdit {
                        media_time: m_b,
                        delay: 0,
                        media_end: None,
                    },
                    ts_b,
                ),
            ];

            let shifts = timeline_shifts(&edits, 600).unwrap();

            // Presentation time of each track's first edit sample, compared without dividing:
            // a/ts_a vs b/ts_b becomes a·ts_b vs b·ts_a, and one tick of the coarser timescale
            // is `max(ts_a, ts_b)` in those units.
            let presented_a = i128::from(shifts[0] + m_a) * i128::from(ts_b);
            let presented_b = i128::from(shifts[1] + m_b) * i128::from(ts_a);
            let tolerance = i128::from(ts_a.max(ts_b));
            assert!(
                (presented_a - presented_b).abs() <= tolerance,
                "{m_a}/{ts_a} vs {m_b}/{ts_b}: {presented_a} vs {presented_b}"
            );
        }
    }

    fn sample(decode_time: u64, duration: u32, composition_offset: i32) -> Sample {
        Sample {
            offset: 0,
            size: 1,
            decode_time,
            duration,
            composition_offset,
            is_sync: true,
        }
    }

    fn track(kind: TrackKind, samples: Vec<Sample>) -> Track {
        Track {
            id: 1,
            key: match kind {
                TrackKind::Video => TrackKey::VIDEO,
                TrackKind::Audio => TrackKey::audio(1),
            },
            kind,
            language: "und".to_owned(),
            timeline_shift: 0,
            timescale: 1000,
            duration: samples
                .iter()
                .map(|sample| u64::from(sample.duration))
                .sum(),
            codec: CodecConfig::Aac {
                sample_rate: 48_000,
                channels: 2,
                object_type: 2,
            },
            samples,
        }
    }

    #[test]
    fn audio_drops_priming_samples_that_end_before_the_edit() {
        let mut audio = track(
            TrackKind::Audio,
            (0..4).map(|index| sample(index * 1024, 1024, 0)).collect(),
        );
        let edit = TrackEdit {
            media_time: 1024,
            delay: 0,
            media_end: None,
        };

        apply(&mut audio, edit, 2176).unwrap();

        assert_eq!(audio.samples.len(), 3);
        assert_eq!(audio.samples[0].decode_time, 1024 + 2176);
        assert_eq!(audio.timeline_shift, 2176);
        assert_eq!(audio.duration, 4 * 1024 + 2176);
    }

    #[test]
    fn audio_keeps_a_sample_the_edit_starts_inside() {
        let mut audio = track(
            TrackKind::Audio,
            (0..3).map(|index| sample(index * 1024, 1024, 0)).collect(),
        );
        let edit = TrackEdit {
            media_time: 1500,
            delay: 0,
            media_end: None,
        };

        apply(&mut audio, edit, 0).unwrap();

        assert_eq!(
            audio.samples.len(),
            2,
            "only the sample ending at 1024 goes"
        );
    }

    #[test]
    fn video_keeps_every_sample_because_none_can_be_dropped_safely() {
        let mut video = track(
            TrackKind::Video,
            (0..3).map(|index| sample(index * 10, 10, 20)).collect(),
        );
        let edit = TrackEdit {
            media_time: 20,
            delay: 0,
            media_end: None,
        };

        apply(&mut video, edit, 0).unwrap();

        assert_eq!(video.samples.len(), 3);
    }

    #[test]
    fn a_plain_end_trim_drops_the_trailing_samples() {
        let mut audio = track(
            TrackKind::Audio,
            (0..5).map(|index| sample(index * 1024, 1024, 0)).collect(),
        );
        let edit = TrackEdit {
            media_time: 0,
            delay: 0,
            media_end: Some(3 * 1024),
        };

        apply(&mut audio, edit, 0).unwrap();

        assert_eq!(audio.samples.len(), 3);
    }

    #[test]
    fn an_end_that_matches_the_media_end_drops_nothing() {
        let mut audio = track(
            TrackKind::Audio,
            (0..3).map(|index| sample(index * 1024, 1024, 0)).collect(),
        );
        let edit = TrackEdit {
            media_time: 0,
            delay: 0,
            media_end: Some(3 * 1024),
        };

        apply(&mut audio, edit, 0).unwrap();

        assert_eq!(audio.samples.len(), 3);
    }

    #[test]
    fn an_end_trim_through_reordered_video_is_rejected() {
        // Decode order I P B: the B presents before the P, so a cut between them is not a suffix.
        let mut video = track(
            TrackKind::Video,
            vec![sample(0, 10, 0), sample(10, 10, 20), sample(20, 10, 0)],
        );
        let edit = TrackEdit {
            media_time: 0,
            delay: 0,
            media_end: Some(25),
        };

        let error = apply(&mut video, edit, 0).expect_err("cut inside reordered run");

        assert!(matches!(error, Error::Unsupported(_)), "{error}");
    }

    #[test]
    fn a_track_with_no_edit_is_left_exactly_as_parsed() {
        let mut audio = track(TrackKind::Audio, vec![sample(0, 1024, 0)]);
        audio.duration = 999_999;
        let before = audio.clone();

        apply(&mut audio, TrackEdit::NONE, 0).unwrap();

        assert_eq!(audio, before);
    }

    #[test]
    fn an_edit_that_removes_every_sample_is_invalid() {
        let mut audio = track(TrackKind::Audio, vec![sample(0, 1024, 0)]);
        let edit = TrackEdit {
            media_time: 5000,
            delay: 0,
            media_end: None,
        };

        assert!(matches!(
            apply(&mut audio, edit, 0),
            Err(Error::InvalidMedia(_))
        ));
    }
}
