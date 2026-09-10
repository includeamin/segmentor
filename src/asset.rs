use std::collections::HashMap;
use std::path::Path;

use bytes::Bytes;

use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{MediaIndex, Track, TrackKind};
use crate::segment::{SegmentPlan, TrackSegment};
use crate::source::{LocalMediaSource, MediaSource};
use crate::{fmp4, mp4, segment};

#[derive(Debug)]
pub(crate) struct PackagedAsset {
    pub(crate) source: LocalMediaSource,
    pub(crate) index: MediaIndex,
    pub(crate) plan: SegmentPlan,
    init_segments: HashMap<TrackKind, Bytes>,
    limits: LimitsConfig,
}

impl PackagedAsset {
    pub(crate) fn load(
        path: impl AsRef<Path>,
        segment_duration_ms: u64,
        limits: &LimitsConfig,
    ) -> Result<Self> {
        let source = LocalMediaSource::open(path)?;
        let index = mp4::parse(&source, limits)?;
        let plan = segment::plan(&index, segment_duration_ms, limits)?;
        let init_segments = index
            .tracks
            .iter()
            .map(|track| {
                fmp4::write_init_segment(&source, track.id)
                    .map(Bytes::from)
                    .map(|bytes| (track.kind, bytes))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        Ok(Self {
            source,
            index,
            plan,
            init_segments,
            limits: limits.clone(),
        })
    }

    pub(crate) fn track(&self, kind: TrackKind) -> Result<&Track> {
        self.index
            .tracks
            .iter()
            .find(|track| track.kind == kind)
            .ok_or(Error::Unsupported("requested track is not present"))
    }

    pub(crate) fn init_segment(&self, kind: TrackKind) -> Result<Bytes> {
        self.init_segments
            .get(&kind)
            .cloned()
            .ok_or(Error::Unsupported("requested track is not present"))
    }

    pub(crate) fn prepare_media_segment(
        &self,
        kind: TrackKind,
        segment_index: u32,
    ) -> Result<fmp4::PreparedSegment> {
        let track = self.track(kind)?;
        let segment = self
            .plan
            .segments
            .get(usize::try_from(segment_index).map_err(|_| {
                Error::InvalidMedia("segment index does not fit in memory".to_owned())
            })?)
            .ok_or_else(|| Error::InvalidMedia("segment does not exist".to_owned()))?;
        let track_segment = segment
            .tracks
            .iter()
            .find(|candidate| candidate.track_id == track.id)
            .copied()
            .ok_or_else(|| Error::InvalidMedia("segment is missing a track".to_owned()))?;
        let sequence_number = segment_index
            .checked_add(1)
            .ok_or_else(|| Error::InvalidMedia("sequence number overflow".to_owned()))?;

        fmp4::prepare_media_segment(track, track_segment, sequence_number, &self.limits)
    }

    pub(crate) fn read_range(&self, range: crate::source::ByteRange) -> Result<Bytes> {
        self.source.read_range(range)
    }

    pub(crate) fn version(&self) -> String {
        self.index
            .source
            .moov_sha256
            .expect("parsed assets always have a moov hash")
            .iter()
            .take(8)
            .fold(String::with_capacity(16), |mut version, byte| {
                use std::fmt::Write;
                write!(version, "{byte:02x}").expect("writing to a String cannot fail");
                version
            })
    }

    pub(crate) fn track_segments(&self, track_id: u32) -> impl Iterator<Item = TrackSegment> + '_ {
        self.plan.segments.iter().filter_map(move |segment| {
            segment
                .tracks
                .iter()
                .find(|candidate| candidate.track_id == track_id)
                .copied()
        })
    }
}
