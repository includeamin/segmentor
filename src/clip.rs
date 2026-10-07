//! Cutting one file's sample index down to a time window, and placing the result on a sequence's
//! timeline. See `docs/technical-design/0008-clipping-and-concatenation.md`.
//!
//! Nothing here reads media or decodes: a clip is a subset of the samples the parser already
//! found, with its decode times moved. The planner, init segments, and fragment writer then treat
//! the result exactly like a whole file.

use crate::error::{Error, Result};
use crate::media::{MediaIndex, Sample, SampleIndex, Track, TrackKind};
use crate::source::SourceIdentity;

/// The largest `from_ms` or `to_ms` accepted (`2^32 − 1`): every conversion to ticks then fits
/// a `u64` at any 32-bit timescale.
pub(crate) const MAX_CLIP_MS: u64 = 4_294_967_295;

const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// A window of one file, on the file's own clock (the one its edit lists describe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClipWindow {
    pub(crate) from_ms: u64,
    /// `None` plays to the end of the file.
    pub(crate) to_ms: Option<u64>,
}

/// A point on a sequence's timeline, kept in nanoseconds so that converting it to each clip's
/// own timescale rounds once per clip and never accumulates across clips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) struct TimelinePosition(u64);

impl TimelinePosition {
    pub(crate) const ZERO: Self = Self(0);

    #[cfg(test)]
    pub(crate) const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    pub(crate) const fn nanos(self) -> u64 {
        self.0
    }

    /// Whole milliseconds, for manifests and logs.
    pub(crate) const fn millis(self) -> u64 {
        self.0 / 1_000_000
    }

    /// This position in `timescale` ticks, rounded down.
    pub(crate) fn in_ticks(self, timescale: u32) -> Result<u64> {
        u64::try_from(u128::from(self.0) * u128::from(timescale) / NANOS_PER_SECOND)
            .map_err(|_| overflow())
    }

    /// The position `ticks` of `timescale` later.
    fn after(self, ticks: u64, timescale: u32) -> Result<Self> {
        let nanos = (u128::from(ticks) * NANOS_PER_SECOND)
            .checked_div(u128::from(timescale))
            .ok_or_else(overflow)?;
        u64::try_from(nanos)
            .ok()
            .and_then(|nanos| self.0.checked_add(nanos))
            .map(Self)
            .ok_or_else(overflow)
    }
}

fn overflow() -> Error {
    Error::InvalidMedia("clip timing overflow".to_owned())
}

/// What happens to audio that runs past the end of a clip's video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trailing {
    /// Kept, as the segment planner keeps it for a whole file: nothing follows this clip.
    Keep,
    /// Stopped where the video ends, so the next clip's audio overlaps by less than one frame.
    Cut,
}

/// One clip's samples, moved onto the sequence timeline.
#[derive(Debug)]
pub(crate) struct Trimmed {
    pub(crate) index: MediaIndex,
    /// Where this clip ends, which is where the next one starts.
    pub(crate) end: TimelinePosition,
    /// The window actually served, on the file's own clock, for the `clip_trimmed` log line.
    pub(crate) served_from_ms: u64,
    pub(crate) served_to_ms: u64,
}

/// Cuts `index` down to `window` and moves every kept sample so the clip starts at `start`
/// (TDD 0008, "Cutting a clip" and "Timeline").
pub(crate) fn trim(
    index: &MediaIndex,
    window: ClipWindow,
    start: TimelinePosition,
    trailing: Trailing,
) -> Result<Trimmed> {
    let reference = reference_track(index)?;
    let timescale = reference.timescale;
    let (first, end) = reference_window(index, reference, window)?;
    let kept = reference.samples.window(first..end);
    let origin = reference.samples.decode_time(first);
    let kept_end = sample_end(&kept.last().expect("a window holds at least one sample"))?;
    let earliest = kept
        .iter()
        .map(|sample| presented(&sample))
        .min()
        .expect("a window holds at least one sample");
    // Negative composition offsets can show a frame before its decode time; moving the clip later
    // by that much keeps every frame at or after the clip's start and every decode time positive.
    let lead = u64::try_from(i128::from(origin) - earliest).unwrap_or(0);
    // Audio past the video's end belongs in the stream only when no clip follows this one.
    let to_the_end = end == reference.samples.len() && trailing == Trailing::Keep;

    let mut tracks = Vec::with_capacity(index.tracks.len());
    for track in &index.tracks {
        let (range, track_origin, track_lead) = if track.id == reference.id {
            ((first, end), origin, lead)
        } else {
            let track_origin = rescale(origin, timescale, track.timescale)?;
            let span_end = rescale(kept_end, timescale, track.timescale)?;
            (
                follower_window(track, track_origin, span_end, to_the_end),
                track_origin,
                rescale(lead, timescale, track.timescale)?,
            )
        };
        let base = start
            .in_ticks(track.timescale)?
            .checked_add(track_lead)
            .ok_or_else(overflow)?;
        tracks.push(moved_track(track, range, track_origin, base)?);
    }

    let length = kept_end
        .checked_sub(origin)
        .and_then(|length| length.checked_add(lead))
        .ok_or_else(overflow)?;
    let offset = index.presentation_offset_ms;
    Ok(Trimmed {
        index: MediaIndex {
            source: index.source.clone(),
            movie_timescale: index.movie_timescale,
            duration: rescale(length, timescale, index.movie_timescale)?,
            presentation_offset_ms: index.presentation_offset_ms,
            tracks,
            skipped_tracks: index.skipped_tracks.clone(),
            fragmentation: index.fragmentation,
        },
        end: start.after(length, timescale)?,
        served_from_ms: ticks_to_ms(u64::try_from(earliest).unwrap_or(0), timescale)
            .saturating_sub(offset),
        served_to_ms: ticks_to_ms(kept_end, timescale).saturating_sub(offset),
    })
}

/// The track cuts are decided on: the video track, or the first audio track when there is no
/// video, exactly as in the segment planner.
fn reference_track(index: &MediaIndex) -> Result<&Track> {
    index
        .tracks
        .iter()
        .find(|track| track.kind == TrackKind::Video)
        .or_else(|| {
            index
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Audio)
        })
        .ok_or_else(|| Error::Unsupported("input has no audio or video track".to_owned()))
}

/// The reference track's kept samples, `first..end`, for `window` read on the file's own clock.
fn reference_window(
    index: &MediaIndex,
    reference: &Track,
    window: ClipWindow,
) -> Result<(usize, usize)> {
    let timescale = reference.timescale;
    let offset = index.presentation_offset_ms;
    let reference_end = sample_end(
        &reference
            .samples
            .last()
            .ok_or_else(|| Error::InvalidMedia("track contains no samples".to_owned()))?,
    )?;
    let from = ms_to_ticks(
        window.from_ms.checked_add(offset).ok_or_else(overflow)?,
        timescale,
    )?;
    if from >= reference_end {
        return Err(Error::InvalidMedia(format!(
            "from_ms {} is at or past the end of the file",
            window.from_ms
        )));
    }
    let to = window
        .to_ms
        .map(|to_ms| ms_to_ticks(to_ms.checked_add(offset).ok_or_else(overflow)?, timescale))
        .transpose()?;
    if to.is_some_and(|to| to <= from) {
        return Err(Error::InvalidMedia(
            "to_ms must be later than from_ms".to_owned(),
        ));
    }
    // A window reaching the end of the file keeps every sample, including the last frames a
    // reorder delay shows after the last decode time; `None` means "to the end".
    let to = to.filter(|&to| to < reference_end);
    Ok(match reference.kind {
        TrackKind::Video => video_window(&reference.samples, from, to),
        TrackKind::Audio => audio_window(&reference.samples, from, to),
    })
}

/// From the last keyframe shown at or before `from` (or the first keyframe, when even that is
/// shown later), through the shortest decode-order prefix holding every frame shown before `to`.
/// A decode-order prefix is always decodable; with B-frames it may keep a frame shown after `to`.
fn video_window(samples: &SampleIndex, from: u64, to: Option<u64>) -> (usize, usize) {
    let from = i128::from(from);
    let first = samples
        .sync_indices()
        .filter(|&index| presented(&samples.get(index)) <= from)
        .last()
        .or_else(|| samples.sync_indices().next())
        .unwrap_or(0);
    let end = to.map_or(samples.len(), |to| {
        let to = i128::from(to);
        samples
            .range(first..samples.len())
            .enumerate()
            .filter(|(_, sample)| presented(sample) < to)
            .last()
            .map_or(first + 1, |(position, _)| first + position + 1)
    });
    (first, end.max(first + 1))
}

/// Every audio frame is a valid cut: from the frame that holds `from` up to the first frame that
/// starts at or after `to`.
fn audio_window(samples: &SampleIndex, from: u64, to: Option<u64>) -> (usize, usize) {
    let first = samples
        .partition_by_decode_time(from.saturating_add(1))
        .saturating_sub(1);
    let end = to.map_or(samples.len(), |to| samples.partition_by_decode_time(to));
    (first, end.max(first + 1))
}

/// Another track's samples inside the reference's kept span, by the rule the segment planner
/// cuts followers with. When the reference is kept to its end, so is every other track.
fn follower_window(track: &Track, start: u64, end: u64, to_the_end: bool) -> (usize, usize) {
    let first = track.samples.partition_by_decode_time(start);
    let last = if to_the_end {
        track.samples.len()
    } else {
        track.samples.partition_by_decode_time(end)
    };
    (first, last.max(first))
}

/// `track` with only `range` kept, each decode time moved from `origin` to `base`.
fn moved_track(track: &Track, range: (usize, usize), origin: u64, base: u64) -> Result<Track> {
    let kept = track.samples.window(range.0..range.1);
    if kept.is_empty() {
        return Err(Error::InvalidMedia(format!(
            "the window leaves track `{}` with no samples",
            track.key
        )));
    }
    // Every kept sample is at or after `origin`, so moving the first one from `origin` to
    // `base` checks them all.
    if kept.decode_time(0) < origin {
        return Err(overflow());
    }
    let samples = kept.shifted(
        i128::from(base) - i128::from(origin),
        "clip timing overflow",
    )?;
    let duration = sample_end(&samples.last().expect("checked non-empty above"))?
        .checked_sub(samples.decode_time(0))
        .ok_or_else(overflow)?;
    Ok(Track {
        id: track.id,
        key: track.key,
        kind: track.kind,
        language: track.language.clone(),
        timeline_shift: track.timeline_shift,
        timescale: track.timescale,
        duration,
        codec: track.codec.clone(),
        samples,
    })
}

/// The URL version of an asset built from clips: the mapper's `version`, every clip's content,
/// window, and own encryption, and the format revision. The window is included even for a single
/// clip, so a trimmed copy never shares a version, and so cached immutable segments, with the
/// whole file. Each clip's encryption is hashed on its own, not once for the whole sequence, so
/// re-keying one clip (or turning its encryption on or off) changes the URL even if no other clip
/// changed (TDD 0009, "Different keys per clip").
pub(crate) fn version_of<'a>(
    mapper_version: &str,
    aes128: Option<&crate::cenc::Aes128>,
    clips: impl IntoIterator<
        Item = (
            &'a SourceIdentity,
            ClipWindow,
            Option<&'a crate::cenc::Encryption>,
        ),
    >,
) -> String {
    use std::fmt::Write;

    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"clips");
    hasher.update(crate::asset::FORMAT_REVISION.to_be_bytes());
    hasher.update((mapper_version.len() as u64).to_be_bytes());
    hasher.update(mapper_version.as_bytes());
    // The whole-segment key covers every clip, so it is hashed once, and only when present: a
    // sequence without it keeps the version it always had.
    if let Some(aes128) = aes128 {
        hasher.update(b"aes128");
        hasher.update(aes128.fingerprint());
    }
    for (source, window, encryption) in clips {
        hasher.update(
            source
                .metadata_hash
                .expect("parsed assets always have a metadata hash"),
        );
        hasher.update(window.from_ms.to_be_bytes());
        match window.to_ms {
            Some(to_ms) => {
                hasher.update([1]);
                hasher.update(to_ms.to_be_bytes());
            }
            None => hasher.update([0]),
        }
        match encryption {
            Some(encryption) => {
                hasher.update(b"cbcs");
                hasher.update(encryption.fingerprint());
            }
            None => hasher.update(b"clear"),
        }
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut version, byte| {
            write!(version, "{byte:02x}").expect("writing to a String cannot fail");
            version
        })
}

fn presented(sample: &Sample) -> i128 {
    i128::from(sample.decode_time) + i128::from(sample.composition_offset)
}

fn sample_end(sample: &Sample) -> Result<u64> {
    sample
        .decode_time
        .checked_add(u64::from(sample.duration))
        .ok_or_else(overflow)
}

fn ms_to_ticks(milliseconds: u64, timescale: u32) -> Result<u64> {
    u64::try_from(u128::from(milliseconds) * u128::from(timescale) / 1000).map_err(|_| overflow())
}

/// For log lines only, so an out-of-range value saturates instead of failing the load.
fn ticks_to_ms(ticks: u64, timescale: u32) -> u64 {
    (u128::from(ticks) * 1000)
        .checked_div(u128::from(timescale))
        .and_then(|milliseconds| u64::try_from(milliseconds).ok())
        .unwrap_or(u64::MAX)
}

fn rescale(value: u64, from_timescale: u32, to_timescale: u32) -> Result<u64> {
    (u128::from(value) * u128::from(to_timescale))
        .checked_div(u128::from(from_timescale))
        .and_then(|scaled| u64::try_from(scaled).ok())
        .ok_or_else(overflow)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::config::LimitsConfig;
    use crate::mp4;
    use crate::source::{LocalMediaSource, MediaSourceKind};

    fn parse(name: &str) -> MediaIndex {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let source = MediaSourceKind::Local(Arc::new(
            LocalMediaSource::open(path).expect("fixture should open"),
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build")
            .block_on(mp4::parse(&source, &LimitsConfig::default()))
            .expect("fixture should parse")
            .index
    }

    fn video(index: &MediaIndex) -> &Track {
        index
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .expect("a video track")
    }

    fn audio(index: &MediaIndex) -> &Track {
        index
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .expect("an audio track")
    }

    fn presented(sample: &Sample) -> i128 {
        i128::from(sample.decode_time) + i128::from(sample.composition_offset)
    }

    const fn window(from_ms: u64, to_ms: Option<u64>) -> ClipWindow {
        ClipWindow { from_ms, to_ms }
    }

    /// Where a kept sample came from in the original track, found by its byte offset.
    fn original_position(original: &Track, kept: &Sample) -> usize {
        original
            .samples
            .all()
            .iter()
            .position(|sample| sample.offset == kept.offset)
            .expect("kept samples come from the original")
    }

    #[test]
    fn the_start_snaps_back_to_the_last_keyframe_shown_at_or_before_from() {
        let index = parse("h264-aac.mp4");

        let trimmed = trim(
            &index,
            window(1500, None),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();

        let kept = video(&trimmed.index);
        // Keyframes are one second apart; the one at 1 s (frame 30) is the last shown by 1.5 s.
        assert_eq!(original_position(video(&index), &kept.samples.all()[0]), 30);
        assert!(kept.samples.all()[0].is_sync);
        assert_eq!(kept.samples.len(), video(&index).samples.len() - 30);
    }

    #[test]
    fn a_from_before_the_first_frame_is_shown_keeps_the_first_keyframe_and_the_whole_file() {
        // Without an edit list, the first frame appears a reorder delay after zero, so no keyframe
        // is shown at or before 0 ms; the clip still starts at the first one, and nothing moves.
        let index = parse("h264-aac.mp4");

        let trimmed = trim(
            &index,
            window(0, None),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();

        for (kept, original) in trimmed.index.tracks.iter().zip(&index.tracks) {
            assert_eq!(kept.samples, original.samples, "track {}", kept.key);
        }
    }

    #[test]
    fn the_end_keeps_every_frame_shown_before_to_as_a_decodable_prefix() {
        let index = parse("h264-aac.mp4");
        let original = video(&index);
        let to_ticks = i128::from(u64::from(original.timescale) * 2500 / 1000);

        let trimmed = trim(
            &index,
            window(0, Some(2500)),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();

        let kept = video(&trimmed.index).samples.len();
        let last_shown_before = original
            .samples
            .all()
            .iter()
            .rposition(|sample| presented(sample) < to_ticks)
            .unwrap();
        assert_eq!(
            kept,
            last_shown_before + 1,
            "the shortest prefix holding every such frame"
        );
        assert!(kept < original.samples.len());
    }

    #[test]
    fn audio_is_cut_at_the_video_cut() {
        let index = parse("h264-aac.mp4");
        let (original_video, original_audio) = (video(&index), audio(&index));

        let trimmed = trim(
            &index,
            window(1500, Some(2500)),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();

        let (kept_video, kept_audio) = (video(&trimmed.index), audio(&trimmed.index));
        let first = &original_video.samples.all()
            [original_position(original_video, &kept_video.samples.all()[0])];
        let last = &original_video.samples.all()
            [original_position(original_video, kept_video.samples.all().last().unwrap())];
        let video_end = last.decode_time + u64::from(last.duration);
        let rescale = |ticks: u64| {
            ticks * u64::from(original_audio.timescale) / u64::from(original_video.timescale)
        };
        let expected_first = original_audio
            .samples
            .all()
            .partition_point(|sample| sample.decode_time < rescale(first.decode_time));
        let expected_end = original_audio
            .samples
            .all()
            .partition_point(|sample| sample.decode_time < rescale(video_end));
        assert_eq!(
            original_position(original_audio, &kept_audio.samples.all()[0]),
            expected_first
        );
        assert_eq!(kept_audio.samples.len(), expected_end - expected_first);
    }

    #[test]
    fn an_audio_only_file_is_cut_at_any_frame() {
        let index = parse("aac-only.m4a");
        let original = audio(&index);
        let ticks =
            |ms: u64| (ms + index.presentation_offset_ms) * u64::from(original.timescale) / 1000;

        let trimmed = trim(
            &index,
            window(1000, Some(2000)),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();

        let kept = audio(&trimmed.index);
        let first = &original.samples.all()[original_position(original, &kept.samples.all()[0])];
        assert!(
            first.decode_time <= ticks(1000)
                && ticks(1000) < first.decode_time + u64::from(first.duration),
            "the first kept frame holds from_ms"
        );
        let end = original_position(original, kept.samples.all().last().unwrap()) + 1;
        assert!(original.samples.all()[end - 1].decode_time < ticks(2000));
        assert!(original.samples.all()[end].decode_time >= ticks(2000));
    }

    #[test]
    fn a_to_past_the_end_is_clamped() {
        let index = parse("h264-aac.mp4");

        let open = trim(
            &index,
            window(1500, None),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();
        let past = trim(
            &index,
            window(1500, Some(10_000_000)),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();

        assert_eq!(open.index.tracks, past.index.tracks);
        assert_eq!(open.end, past.end);
    }

    #[test]
    fn a_from_at_or_past_the_end_is_an_error() {
        let index = parse("h264-aac.mp4");

        let error = trim(
            &index,
            window(5000, None),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap_err();

        assert!(error.to_string().contains("from_ms"), "{error}");
    }

    #[test]
    fn from_ms_is_read_on_the_files_own_clock() {
        // Both files hold the same frames. Without an edit list, frame 30 is shown at 1.067 s, so
        // 1000 ms falls in the first GOP; ffmpeg's default edit list shows it at exactly 1.000 s.
        let plain = parse("h264-aac.mp4");
        let edited = parse("h264-aac-default-edits.mp4");
        assert!(
            presented(&video(&plain).samples.all()[30]) > 15_360,
            "premise"
        );
        assert!(edited.presentation_offset_ms > 0, "premise");

        let from_plain = trim(
            &plain,
            window(1000, None),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();
        let from_edited = trim(
            &edited,
            window(1000, None),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();

        assert_eq!(
            original_position(video(&plain), &video(&from_plain.index).samples.all()[0]),
            0
        );
        assert_eq!(
            original_position(video(&edited), &video(&from_edited.index).samples.all()[0]),
            30
        );
    }

    #[test]
    fn a_clip_is_placed_at_the_given_start_and_ends_where_its_video_does() {
        let index = parse("h264-aac.mp4");
        let start = TimelinePosition::from_nanos(2_500_000_000);

        let trimmed = trim(&index, window(1500, None), start, Trailing::Keep).unwrap();

        let kept_video = video(&trimmed.index);
        assert_eq!(
            kept_video.samples.all()[0].decode_time,
            38_400,
            "2.5 s at 15360 ticks per second"
        );
        let original_audio = audio(&index);
        let kept_audio = audio(&trimmed.index);
        let original_first = original_audio.samples.all()
            [original_position(original_audio, &kept_audio.samples.all()[0])]
        .decode_time;
        // Audio moves by exactly as much as video (1 s to 2.5 s): its offset from the cut is kept.
        assert_eq!(
            kept_audio.samples.all()[0].decode_time - 120_000,
            original_first - 48_000
        );
        let last = kept_video.samples.last().unwrap();
        assert_eq!(
            trimmed.end.in_ticks(15_360).unwrap(),
            last.decode_time + u64::from(last.duration)
        );
    }

    #[test]
    fn a_negative_composition_offset_moves_the_clip_so_nothing_shows_before_its_start() {
        let mut index = parse("h264-aac.mp4");
        let track = index
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap();
        // Every frame shown 256 ticks before it is decoded.
        let mut samples = track.samples.all();
        for sample in &mut samples {
            sample.composition_offset = -256;
        }
        track.samples = SampleIndex::from(samples);
        let start = TimelinePosition::from_nanos(1_000_000_000);

        let trimmed = trim(&index, window(0, None), start, Trailing::Keep).unwrap();

        let kept = video(&trimmed.index);
        let earliest = kept.samples.all().iter().map(presented).min().unwrap();
        assert_eq!(
            earliest, 15_360,
            "the first frame is shown exactly at the clip's start"
        );
        assert_eq!(kept.samples.all()[0].decode_time, 15_360 + 256);
    }

    #[test]
    fn a_one_frame_window_still_plays() {
        let index = parse("h264-aac.mp4");
        // Just after keyframe 30 is shown, and 4 ms long: less than one frame interval (33 ms).
        let shown = u64::try_from(presented(&video(&index).samples.all()[30])).unwrap();
        let from_ms = (shown * 1000).div_ceil(15_360);

        let trimmed = trim(
            &index,
            window(from_ms, Some(from_ms + 4)),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();

        assert_eq!(video(&trimmed.index).samples.len(), 1);
        assert!(
            !audio(&trimmed.index).samples.is_empty(),
            "the audio under that frame is kept"
        );
    }

    #[test]
    fn the_version_covers_the_mapper_version_and_every_clips_content_and_window() {
        let index = parse("h264-aac.mp4");
        let other = parse("hevc-aac.mp4");
        let source = &index.source;
        let base = version_of("v1", None, [(source, window(0, Some(2000)), None)]);

        assert_eq!(base.len(), 16);
        assert_eq!(
            base,
            version_of("v1", None, [(source, window(0, Some(2000)), None)]),
            "stable"
        );
        assert_ne!(
            base,
            version_of("v1", None, [(source, window(0, Some(3000)), None)]),
            "the window"
        );
        assert_ne!(
            base,
            version_of("v1", None, [(source, window(0, None), None)]),
            "an open end"
        );
        assert_ne!(
            base,
            version_of("v2", None, [(source, window(0, Some(2000)), None)]),
            "the mapper version"
        );
        assert_ne!(
            base,
            version_of("v1", None, [(&other.source, window(0, Some(2000)), None)]),
            "the content"
        );
        assert_ne!(
            base,
            version_of(
                "v1",
                None,
                [
                    (source, window(0, Some(2000)), None),
                    (source, window(0, Some(2000)), None)
                ],
            ),
            "the clip count"
        );
    }

    #[test]
    fn the_version_covers_each_clips_own_encryption() {
        let index = parse("h264-aac.mp4");
        let source = &index.source;
        let one = crate::cenc::tests_support::sample_encryption();
        let other = crate::cenc::tests_support::rekeyed_encryption();
        let clear = version_of("v1", None, [(source, window(0, None), None)]);
        let encrypted = version_of("v1", None, [(source, window(0, None), Some(&one))]);
        let rekeyed = version_of("v1", None, [(source, window(0, None), Some(&other))]);

        assert_ne!(
            clear, encrypted,
            "turning encryption on changes the version"
        );
        assert_ne!(encrypted, rekeyed, "a different key changes the version");

        // Two clips: only the second is encrypted. Changing just that clip's key must still
        // change the whole sequence's version, even though clip 0 and the window list match.
        let base = version_of(
            "v1",
            None,
            [
                (source, window(0, Some(1000)), None),
                (source, window(1000, None), Some(&one)),
            ],
        );
        let rekeyed_second = version_of(
            "v1",
            None,
            [
                (source, window(0, Some(1000)), None),
                (source, window(1000, None), Some(&other)),
            ],
        );
        assert_ne!(
            base, rekeyed_second,
            "re-keying one clip changes the version even when the other clip is unchanged"
        );
    }

    #[test]
    fn only_the_last_clip_keeps_audio_that_outlasts_its_video() {
        let index = parse("h264-aac.mp4");
        let last_frame = video(&index).samples.last().unwrap();
        let video_end_in_audio_ticks =
            (last_frame.decode_time + u64::from(last_frame.duration)) * 48_000 / 15_360;
        assert!(
            audio(&index).samples.all().last().unwrap().decode_time >= video_end_in_audio_ticks,
            "premise: an audio frame starts after the video ends"
        );

        let middle = trim(
            &index,
            window(0, None),
            TimelinePosition::ZERO,
            Trailing::Cut,
        )
        .unwrap();
        let last = trim(
            &index,
            window(0, None),
            TimelinePosition::ZERO,
            Trailing::Keep,
        )
        .unwrap();

        assert!(
            audio(&middle.index)
                .samples
                .all()
                .last()
                .unwrap()
                .decode_time
                < video_end_in_audio_ticks,
            "a clip followed by another stops its audio where its video ends"
        );
        assert_eq!(audio(&last.index).samples, audio(&index).samples);
    }
}
