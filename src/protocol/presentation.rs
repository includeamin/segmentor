//! The read-only view of a media file that protocol renderers work from.
//!
//! Renderers need tracks, the segment plan, and the version string, and nothing else. Passing
//! this view instead of the whole loaded asset keeps `protocol` independent of `asset`.

use crate::error::{Error, Result};
use crate::media::{Sample, Track, TrackKind};
use crate::segment::{SegmentPlan, TrackSegment};

/// Bits per second for one track, in the terms HLS and DASH declare them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bandwidth {
    pub(crate) average: u64,
    pub(crate) peak: u64,
}

/// Borrowed tracks, plan, and version of one media file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Presentation<'a> {
    tracks: &'a [Track],
    plan: &'a SegmentPlan,
    version: &'a str,
}

impl<'a> Presentation<'a> {
    pub(crate) const fn new(tracks: &'a [Track], plan: &'a SegmentPlan, version: &'a str) -> Self {
        Self {
            tracks,
            plan,
            version,
        }
    }

    pub(crate) const fn tracks(&self) -> &'a [Track] {
        self.tracks
    }

    pub(crate) const fn version(&self) -> &'a str {
        self.version
    }

    pub(crate) fn track(&self, kind: TrackKind) -> Result<&'a Track> {
        self.tracks
            .iter()
            .find(|track| track.kind == kind)
            .ok_or(Error::NotFound("track does not exist"))
    }

    /// One track's segments in playback order.
    pub(crate) fn track_segments(&self, track_id: u32) -> impl Iterator<Item = TrackSegment> + 'a {
        self.plan.segments.iter().filter_map(move |segment| {
            segment
                .tracks
                .iter()
                .find(|candidate| candidate.track_id == track_id)
                .copied()
        })
    }

    /// Average is total payload over track duration; peak is the burstiest segment.
    pub(crate) fn bandwidth(&self, track: &Track) -> Result<Bandwidth> {
        let overflow = || Error::InvalidMedia("bandwidth calculation overflow".to_owned());
        let rate = |bytes: u64, duration: u64| {
            bytes
                .checked_mul(8)?
                .checked_mul(u64::from(track.timescale))?
                .checked_div(duration)
        };
        let average = rate(payload_bytes(&track.samples)?, track.duration).ok_or_else(overflow)?;
        let mut peak = average;
        for segment in self.track_segments(track.id) {
            let samples = track
                .samples
                .get(segment.first_sample..segment.end_sample)
                .ok_or_else(|| Error::InvalidMedia("segment sample range is invalid".to_owned()))?;
            if let Some(segment_rate) = rate(payload_bytes(samples)?, segment.duration) {
                peak = peak.max(segment_rate);
            }
        }
        Ok(Bandwidth { average, peak })
    }
}

fn payload_bytes(samples: &[Sample]) -> Result<u64> {
    samples.iter().try_fold(0u64, |total, sample| {
        total
            .checked_add(u64::from(sample.size))
            .ok_or_else(|| Error::InvalidMedia("track size overflow".to_owned()))
    })
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Builds a presentation from the committed fixture, without loading a full asset.

    use std::path::PathBuf;

    use super::Presentation;
    use crate::config::LimitsConfig;
    use crate::media::MediaIndex;
    use crate::segment::SegmentPlan;
    use crate::source::LocalMediaSource;
    use crate::{mp4, segment};

    pub(crate) struct Loaded {
        pub(crate) index: MediaIndex,
        pub(crate) plan: SegmentPlan,
        pub(crate) version: String,
    }

    impl Loaded {
        pub(crate) fn h264_aac() -> Self {
            let path =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/h264-aac.mp4");
            let limits = LimitsConfig::default();
            let source = LocalMediaSource::open(path).expect("fixture should open");
            let index = mp4::parse(&source, &limits).expect("fixture should parse");
            let plan = segment::plan(&index, 1000, &limits).expect("fixture should plan");
            Self {
                index,
                plan,
                version: "0123456789abcdef".to_owned(),
            }
        }

        pub(crate) fn presentation(&self) -> Presentation<'_> {
            Presentation::new(&self.index.tracks, &self.plan, &self.version)
        }
    }
}
