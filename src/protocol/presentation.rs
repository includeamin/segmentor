//! The read-only view of a media file that protocol renderers work from.
//!
//! Renderers need tracks, the segment plan, and the version string, and nothing else. Passing
//! this view instead of the whole loaded asset keeps `protocol` independent of `asset`.

use crate::error::{Error, Result};
use crate::media::{Track, TrackKey, TrackKind};
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
    encryption: Option<&'a crate::cenc::Encryption>,
    aes128: Option<&'a crate::cenc::Aes128>,
    muxed: bool,
}

impl<'a> Presentation<'a> {
    pub(crate) const fn new(tracks: &'a [Track], plan: &'a SegmentPlan, version: &'a str) -> Self {
        Self {
            tracks,
            plan,
            version,
            subtitles: &[],
            encryption: None,
            aes128: None,
            muxed: false,
        }
    }

    /// The same view, with every HLS media segment encrypted whole under `aes128` (TDD 0012).
    pub(crate) const fn with_aes128(self, aes128: Option<&'a crate::cenc::Aes128>) -> Self {
        Self { aes128, ..self }
    }

    pub(crate) const fn aes128(&self) -> Option<&'a crate::cenc::Aes128> {
        self.aes128
    }

    /// The same view, with HLS video segments carrying the default audio track (TDD 0011).
    pub(crate) const fn with_muxed_audio(self, muxed: bool) -> Self {
        Self { muxed, ..self }
    }

    /// Whether HLS video segments carry the default (first) audio track.
    pub(crate) const fn muxed_audio(&self) -> bool {
        self.muxed
    }

    /// The same view, encrypted with `encryption` (TDD 0009).
    pub(crate) const fn with_encryption(
        self,
        encryption: Option<&'a crate::cenc::Encryption>,
    ) -> Self {
        Self { encryption, ..self }
    }

    pub(crate) const fn encryption(&self) -> Option<&'a crate::cenc::Encryption> {
        self.encryption
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
        let samples = &track.samples;
        let average =
            rate(samples.payload_bytes(0..samples.len()), track.duration).ok_or_else(overflow)?;
        let mut peak = average;
        for segment in self.track_segments(track.id) {
            if segment.first_sample > segment.end_sample || segment.end_sample > samples.len() {
                return Err(Error::InvalidMedia(
                    "segment sample range is invalid".to_owned(),
                ));
            }
            let bytes = samples.payload_bytes(segment.first_sample..segment.end_sample);
            if let Some(segment_rate) = rate(bytes, segment.duration) {
                peak = peak.max(segment_rate);
            }
        }
        Ok(Bandwidth { average, peak })
    }
}

/// One clip of a sequence, as the renderers see it: its own view, the global number of its first
/// segment, and where it starts on the sequence's timeline.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SequenceClip<'a> {
    pub(crate) presentation: Presentation<'a>,
    pub(crate) first_segment: u32,
    pub(crate) start_nanos: u64,
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
