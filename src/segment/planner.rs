use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{MediaIndex, Track, TrackKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegmentPlan {
    pub(crate) segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) index: u32,
    pub(crate) tracks: Vec<TrackSegment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrackSegment {
    pub(crate) track_id: u32,
    pub(crate) first_sample: usize,
    pub(crate) end_sample: usize,
    pub(crate) decode_time: u64,
    pub(crate) duration: u64,
}

pub(crate) fn plan(
    index: &MediaIndex,
    target_duration_ms: u64,
    limits: &LimitsConfig,
) -> Result<SegmentPlan> {
    let video_tracks = index
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Video)
        .collect::<Vec<_>>();
    let audio_tracks = index
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Audio)
        .collect::<Vec<_>>();
    if video_tracks.len() > 1 {
        return Err(Error::Unsupported(format!(
            "input must contain at most one video track, found {}",
            video_tracks.len()
        )));
    }

    // The reference track decides where segments are cut: the video track, whose cuts must fall
    // on keyframes, or, with no video, the first audio track, where every sample is a valid cut.
    // The remaining audio tracks are cut at the same instants.
    let reference = video_tracks
        .first()
        .or_else(|| audio_tracks.first())
        .copied()
        .ok_or_else(|| Error::Unsupported("input has no audio or video track".to_owned()))?;
    let followers = audio_tracks
        .iter()
        .filter(|track| track.id != reference.id)
        .copied()
        .collect::<Vec<_>>();
    if reference.samples.is_empty() || !reference.samples[0].is_sync {
        return Err(Error::InvalidMedia(
            "the first track must begin with a sync sample".to_owned(),
        ));
    }
    let target_ticks = target_duration_ms
        .checked_mul(u64::from(reference.timescale))
        .and_then(|duration| duration.checked_div(1000))
        .filter(|duration| *duration != 0)
        .ok_or_else(|| Error::InvalidMedia("invalid target segment duration".to_owned()))?;
    let boundaries = reference_boundaries(reference, target_ticks);
    let reference_end = track_end(reference)?;

    let mut segments = Vec::with_capacity(boundaries.len().saturating_sub(1));
    for (segment_index, boundaries) in boundaries.windows(2).enumerate() {
        let first_sample = boundaries[0];
        let end_sample = boundaries[1];
        let decode_time = reference.samples[first_sample].decode_time;
        let end_time = if end_sample == reference.samples.len() {
            reference_end
        } else {
            reference.samples[end_sample].decode_time
        };
        let mut tracks = vec![TrackSegment {
            track_id: reference.id,
            first_sample,
            end_sample,
            decode_time,
            duration: end_time - decode_time,
        }];

        for audio in &followers {
            tracks.push(audio_segment(
                audio,
                reference,
                decode_time,
                end_time,
                end_sample == reference.samples.len(),
            )?);
        }
        if tracks
            .iter()
            .any(|track| track.end_sample - track.first_sample > limits.max_samples_per_segment)
        {
            return Err(Error::InvalidMedia(
                "segment sample count exceeds configured limit".to_owned(),
            ));
        }

        segments.push(Segment {
            index: u32::try_from(segment_index)
                .map_err(|_| Error::InvalidMedia("segment index overflow".to_owned()))?,
            tracks,
        });
    }

    Ok(SegmentPlan { segments })
}

/// Sample indexes at which segments start, plus the end: the first sync sample at or after each
/// target duration.
fn reference_boundaries(reference: &Track, target_ticks: u64) -> Vec<usize> {
    let mut boundaries = vec![0];
    let mut current = 0usize;
    loop {
        let target = reference.samples[current]
            .decode_time
            .saturating_add(target_ticks);
        let Some(next) = reference
            .samples
            .iter()
            .enumerate()
            .skip(current + 1)
            .find(|(_, sample)| sample.is_sync && sample.decode_time >= target)
            .map(|(index, _)| index)
        else {
            break;
        };
        boundaries.push(next);
        current = next;
    }
    boundaries.push(reference.samples.len());
    boundaries
}

fn audio_segment(
    audio: &Track,
    video: &Track,
    video_start: u64,
    video_end: u64,
    is_final_segment: bool,
) -> Result<TrackSegment> {
    let start = rescale(video_start, video.timescale, audio.timescale)?;
    let end = rescale(video_end, video.timescale, audio.timescale)?;
    let first_sample = audio
        .samples
        .partition_point(|sample| sample.decode_time < start);
    let end_sample = if is_final_segment {
        audio.samples.len()
    } else {
        audio
            .samples
            .partition_point(|sample| sample.decode_time < end)
    };
    let decode_time = audio
        .samples
        .get(first_sample)
        .map_or(start, |sample| sample.decode_time);
    let actual_end = if end_sample == audio.samples.len() {
        track_end(audio)?
    } else {
        audio.samples[end_sample].decode_time
    };

    Ok(TrackSegment {
        track_id: audio.id,
        first_sample,
        end_sample,
        decode_time,
        duration: actual_end.saturating_sub(decode_time),
    })
}

fn rescale(value: u64, from_timescale: u32, to_timescale: u32) -> Result<u64> {
    value
        .checked_mul(u64::from(to_timescale))
        .and_then(|scaled| scaled.checked_div(u64::from(from_timescale)))
        .ok_or_else(|| Error::InvalidMedia("timestamp rescaling overflow".to_owned()))
}

fn track_end(track: &Track) -> Result<u64> {
    let last = track
        .samples
        .last()
        .ok_or_else(|| Error::InvalidMedia("track contains no samples".to_owned()))?;
    last.decode_time
        .checked_add(u64::from(last.duration))
        .ok_or_else(|| Error::InvalidMedia("track duration overflow".to_owned()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::mp4;
    use crate::source::{LocalMediaSource, MediaSourceKind};

    fn plan_of(name: &str, target_ms: u64) -> (MediaIndex, SegmentPlan) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let source = MediaSourceKind::Local(std::sync::Arc::new(
            LocalMediaSource::open(path).expect("fixture should open"),
        ));
        let limits = LimitsConfig::default();
        let index = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build")
            .block_on(mp4::parse(&source, &limits))
            .expect("fixture should parse")
            .index;
        let plan = plan(&index, target_ms, &limits).expect("fixture should be segmentable");
        (index, plan)
    }

    #[test]
    fn an_audio_only_asset_is_cut_into_windows_of_the_target_duration() {
        let (index, plan) = plan_of("aac-only.m4a", 1000);

        let audio = &index.tracks[0];
        assert_eq!(plan.segments.len(), 3);
        // Cut points are one second apart to within one AAC frame (1024 / 48000 s).
        for segment in &plan.segments[..2] {
            let duration = segment.tracks[0].duration;
            assert!(
                duration.abs_diff(u64::from(audio.timescale)) < 1024,
                "segment lasts {duration} ticks"
            );
        }
        // Every sample lands in exactly one segment.
        let covered = plan
            .segments
            .iter()
            .map(|s| s.tracks[0].end_sample - s.tracks[0].first_sample);
        assert_eq!(covered.sum::<usize>(), audio.samples.len());
    }

    #[test]
    fn audio_only_tracks_after_the_first_follow_its_cuts() {
        let (_, plan) = plan_of("aac-two-tracks-only.m4a", 1000);

        for segment in &plan.segments {
            assert_eq!(segment.tracks.len(), 2);
            assert_eq!(
                segment.tracks[0].first_sample,
                segment.tracks[1].first_sample
            );
            assert_eq!(segment.tracks[0].end_sample, segment.tracks[1].end_sample);
        }
    }

    #[test]
    fn every_audio_track_is_planned_alongside_the_video() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/h264-aac-two-audio.mp4");
        let source = MediaSourceKind::Local(std::sync::Arc::new(
            LocalMediaSource::open(path).expect("fixture should open"),
        ));
        let limits = LimitsConfig::default();
        let index = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build")
            .block_on(mp4::parse(&source, &limits))
            .expect("fixture should parse")
            .index;

        let plan = plan(&index, 1000, &limits).expect("fixture should be segmentable");

        assert_eq!(plan.segments.len(), 3);
        for segment in &plan.segments {
            let ids = segment
                .tracks
                .iter()
                .map(|track| track.track_id)
                .collect::<Vec<_>>();
            assert_eq!(ids, [1, 2, 3], "video, then both audio tracks");
        }
        // The two audio tracks carry the same durations, so they cut identically.
        for segment in &plan.segments {
            assert_eq!(
                segment.tracks[1].first_sample,
                segment.tracks[2].first_sample
            );
            assert_eq!(segment.tracks[1].end_sample, segment.tracks[2].end_sample);
        }
    }

    #[test]
    fn creates_three_keyframe_aligned_segments() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/h264-aac.mp4");
        let source = MediaSourceKind::Local(std::sync::Arc::new(
            LocalMediaSource::open(path).expect("fixture should open"),
        ));
        let limits = LimitsConfig::default();
        let index = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build")
            .block_on(mp4::parse(&source, &limits))
            .expect("fixture should parse")
            .index;

        let plan = plan(&index, 1000, &limits).expect("fixture should be segmentable");

        assert_eq!(plan.segments.len(), 3);
        let video = index
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap();
        let video_segments = plan
            .segments
            .iter()
            .map(|segment| {
                segment
                    .tracks
                    .iter()
                    .find(|track| track.track_id == video.id)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            video_segments
                .iter()
                .map(|segment| segment.first_sample)
                .collect::<Vec<_>>(),
            vec![0, 30, 60]
        );
        assert!(
            video_segments
                .iter()
                .all(|segment| video.samples[segment.first_sample].is_sync)
        );
        assert_eq!(
            video_segments.last().unwrap().end_sample,
            video.samples.len()
        );

        let audio = index
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap();
        let audio_segments = plan
            .segments
            .iter()
            .map(|segment| {
                segment
                    .tracks
                    .iter()
                    .find(|track| track.track_id == audio.id)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(audio_segments[0].first_sample, 0);
        assert_eq!(
            audio_segments.last().unwrap().end_sample,
            audio.samples.len()
        );
        assert!(
            audio_segments
                .windows(2)
                .all(|pair| pair[0].end_sample == pair[1].first_sample)
        );
    }
}
