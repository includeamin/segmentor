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
    if video_tracks.len() != 1 {
        return Err(Error::Unsupported(
            "input must contain exactly one video track",
        ));
    }
    if audio_tracks.len() > 1 {
        return Err(Error::Unsupported(
            "input must contain at most one audio track",
        ));
    }

    let video = video_tracks[0];
    if video.samples.is_empty() || !video.samples[0].is_sync {
        return Err(Error::InvalidMedia(
            "video must begin with a sync sample".to_owned(),
        ));
    }
    let target_ticks = target_duration_ms
        .checked_mul(u64::from(video.timescale))
        .and_then(|duration| duration.checked_div(1000))
        .filter(|duration| *duration != 0)
        .ok_or_else(|| Error::InvalidMedia("invalid target segment duration".to_owned()))?;
    let boundaries = video_boundaries(video, target_ticks);
    let video_end = track_end(video)?;

    let mut segments = Vec::with_capacity(boundaries.len().saturating_sub(1));
    for (segment_index, boundaries) in boundaries.windows(2).enumerate() {
        let first_sample = boundaries[0];
        let end_sample = boundaries[1];
        let decode_time = video.samples[first_sample].decode_time;
        let end_time = if end_sample == video.samples.len() {
            video_end
        } else {
            video.samples[end_sample].decode_time
        };
        let mut tracks = vec![TrackSegment {
            track_id: video.id,
            first_sample,
            end_sample,
            decode_time,
            duration: end_time - decode_time,
        }];

        if let Some(audio) = audio_tracks.first() {
            tracks.push(audio_segment(
                audio,
                video,
                decode_time,
                end_time,
                end_sample == video.samples.len(),
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

fn video_boundaries(video: &Track, target_ticks: u64) -> Vec<usize> {
    let mut boundaries = vec![0];
    let mut current = 0usize;
    loop {
        let target = video.samples[current]
            .decode_time
            .saturating_add(target_ticks);
        let Some(next) = video
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
    boundaries.push(video.samples.len());
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
    use crate::source::LocalMediaSource;

    #[test]
    fn creates_three_keyframe_aligned_segments() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/h264-aac.mp4");
        let source = LocalMediaSource::open(path).expect("fixture should open");
        let limits = LimitsConfig::default();
        let index = mp4::parse(&source, &limits).expect("fixture should parse");

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
