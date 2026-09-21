//! The read-only view of a media file that protocol renderers work from.
//!
//! Renderers need tracks, the segment plan, and the version string, and nothing else. Passing
//! this view instead of the whole loaded asset keeps `protocol` independent of `asset`.

use crate::error::{Error, Result};
use crate::media::{Sample, Track, TrackKey, TrackKind};
use crate::segment::{SegmentPlan, TrackSegment};
use crate::subtitle::Subtitle;

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
    subtitles: &'a [Subtitle],
}

impl<'a> Presentation<'a> {
    pub(crate) const fn new(tracks: &'a [Track], plan: &'a SegmentPlan, version: &'a str) -> Self {
        Self {
            tracks,
            plan,
            version,
            subtitles: &[],
        }
    }

    /// The same view with the asset's sidecar subtitles.
    pub(crate) const fn with_subtitles(self, subtitles: &'a [Subtitle]) -> Self {
        Self { subtitles, ..self }
    }

    pub(crate) const fn subtitles(&self) -> &'a [Subtitle] {
        self.subtitles
    }

    /// The presentation's length in seconds, rounded up: the longest track.
    pub(crate) fn duration_seconds(&self) -> u64 {
        self.tracks
            .iter()
            .map(|track| track.duration.div_ceil(u64::from(track.timescale)))
            .max()
            .unwrap_or(0)
    }

    pub(crate) const fn tracks(&self) -> &'a [Track] {
        self.tracks
    }

    pub(crate) const fn version(&self) -> &'a str {
        self.version
    }

    pub(crate) fn track(&self, key: TrackKey) -> Result<&'a Track> {
        self.tracks
            .iter()
            .find(|track| track.key == key)
            .ok_or(Error::NotFound("track does not exist"))
    }

    /// The video track, absent in an audio-only asset.
    pub(crate) fn video(&self) -> Option<&'a Track> {
        self.tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
    }

    /// Audio tracks in file order; the first is the default rendition.
    pub(crate) fn audio_tracks(&self) -> impl Iterator<Item = &'a Track> + use<'a> {
        self.tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
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
    use crate::source::{LocalMediaSource, MediaSourceKind};
    use crate::{mp4, segment};

    pub(crate) struct Loaded {
        pub(crate) index: MediaIndex,
        pub(crate) plan: SegmentPlan,
        pub(crate) version: String,
    }

    impl Loaded {
        pub(crate) fn h264_aac() -> Self {
            Self::fixture("h264-aac.mp4")
        }

        pub(crate) fn fixture(name: &str) -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name);
            let limits = LimitsConfig::default();
            let source = MediaSourceKind::Local(std::sync::Arc::new(
                LocalMediaSource::open(path).expect("fixture should open"),
            ));
            let index = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime should build")
                .block_on(mp4::parse(&source, &limits))
                .expect("fixture should parse")
                .index;
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
