use std::collections::HashMap;
use std::path::Path;

use bytes::Bytes;

use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{MediaIndex, Sample, Track, TrackKind};
use crate::mp4::ParsedMedia;
use crate::protocol::{Presentation, dash, hls};
use crate::segment::SegmentPlan;
use crate::source::{ByteRange, LocalMediaSource, MediaSourceKind};
use crate::{fmp4, mp4, segment};
use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct PackagedAsset {
    source: MediaSourceKind,
    pub(crate) index: MediaIndex,
    pub(crate) plan: SegmentPlan,
    init_segments: HashMap<TrackKind, Bytes>,
    limits: LimitsConfig,
    version: String,
    rendered: RenderedManifests,
}

/// Playlists and manifests rendered once at load so requests never walk sample tables.
#[derive(Debug, Default)]
struct RenderedManifests {
    hls_master: Bytes,
    hls_media: HashMap<TrackKind, Bytes>,
    dash: Bytes,
}

impl PackagedAsset {
    /// Opens a local file and loads it; a convenience for the CLI, tests, and benchmarks.
    pub(crate) async fn load_local(
        path: impl AsRef<Path>,
        segment_duration_ms: u64,
        limits: &LimitsConfig,
    ) -> Result<Self> {
        let source = MediaSourceKind::Local(Arc::new(LocalMediaSource::open(path)?));
        Self::load(source, segment_duration_ms, limits).await
    }

    /// Fetches metadata from `source`, then plans, writes init segments, and renders playlists.
    ///
    /// Reads are async; the CPU-bound assembly runs on the blocking pool so it never stalls the
    /// async workers.
    pub(crate) async fn load(
        source: MediaSourceKind,
        segment_duration_ms: u64,
        limits: &LimitsConfig,
    ) -> Result<Self> {
        let parsed = mp4::parse(&source, limits).await?;
        let limits = limits.clone();
        tokio::task::spawn_blocking(move || {
            Self::assemble(source, parsed, segment_duration_ms, &limits)
        })
        .await
        .map_err(|error| Error::Io(std::io::Error::other(error)))?
    }

    fn assemble(
        source: MediaSourceKind,
        parsed: ParsedMedia,
        segment_duration_ms: u64,
        limits: &LimitsConfig,
    ) -> Result<Self> {
        let ParsedMedia { index, metadata } = parsed;
        let plan = segment::plan(&index, segment_duration_ms, limits)?;
        let init_segments = index
            .tracks
            .iter()
            .map(|track| {
                fmp4::write_init_segment(&metadata, track.id)
                    .map(Bytes::from)
                    .map(|bytes| (track.kind, bytes))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let version = version_of(&index);
        let rendered =
            RenderedManifests::render(Presentation::new(&index.tracks, &plan, &version))?;
        Ok(Self {
            source,
            index,
            plan,
            init_segments,
            limits: limits.clone(),
            version,
            rendered,
        })
    }

    pub(crate) fn track(&self, kind: TrackKind) -> Result<&Track> {
        self.presentation().track(kind)
    }

    /// The read-only view renderers work from.
    pub(crate) fn presentation(&self) -> Presentation<'_> {
        Presentation::new(&self.index.tracks, &self.plan, &self.version)
    }

    pub(crate) fn init_segment(&self, kind: TrackKind) -> Result<Bytes> {
        self.init_segments
            .get(&kind)
            .cloned()
            .ok_or(Error::NotFound("track does not exist"))
    }

    pub(crate) fn hls_master_playlist(&self) -> Bytes {
        self.rendered.hls_master.clone()
    }

    pub(crate) fn hls_media_playlist(&self, kind: TrackKind) -> Result<Bytes> {
        self.rendered
            .hls_media
            .get(&kind)
            .cloned()
            .ok_or(Error::NotFound("track does not exist"))
    }

    pub(crate) fn dash_manifest(&self) -> Bytes {
        self.rendered.dash.clone()
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
            .ok_or(Error::NotFound("segment does not exist"))?;
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

    pub(crate) async fn read_range(&self, range: ByteRange) -> Result<Bytes> {
        self.source.read_range(range).await
    }

    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    /// Estimated resident bytes held for this asset, used to bound total index memory.
    pub(crate) fn index_bytes(&self) -> u64 {
        let samples = self
            .index
            .tracks
            .iter()
            .map(|track| track.samples.len())
            .sum::<usize>();
        let init = self.init_segments.values().map(Bytes::len).sum::<usize>();
        let rendered = self.rendered.hls_master.len()
            + self.rendered.dash.len()
            + self
                .rendered
                .hls_media
                .values()
                .map(Bytes::len)
                .sum::<usize>();
        (samples.saturating_mul(std::mem::size_of::<Sample>()) as u64)
            .saturating_add(init as u64)
            .saturating_add(rendered as u64)
    }
}

impl RenderedManifests {
    fn render(presentation: Presentation<'_>) -> Result<Self> {
        let hls_media = presentation
            .tracks()
            .iter()
            .map(|track| {
                hls::media_playlist(presentation, track.kind)
                    .map(|playlist| (track.kind, Bytes::from(playlist)))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        Ok(Self {
            hls_master: Bytes::from(hls::master_playlist(presentation)?),
            hls_media,
            dash: Bytes::from(dash::manifest(presentation)?),
        })
    }
}

fn version_of(index: &MediaIndex) -> String {
    use std::fmt::Write;

    index
        .source
        .moov_sha256
        .expect("parsed assets always have a moov hash")
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut version, byte| {
            write!(version, "{byte:02x}").expect("writing to a String cannot fail");
            version
        })
}
