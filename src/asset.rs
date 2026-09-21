use std::collections::HashMap;
use std::path::Path;

use bytes::Bytes;

use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{MediaIndex, Sample, Track, TrackKey};
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
    init_segments: HashMap<TrackKey, Bytes>,
    limits: LimitsConfig,
    version: String,
    rendered: RenderedManifests,
}

/// Playlists and manifests rendered once at load so requests never walk sample tables.
#[derive(Debug, Default)]
struct RenderedManifests {
    hls_master: Bytes,
    hls_media: HashMap<TrackKey, Bytes>,
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
                    .map(|bytes| (track.key, bytes))
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

    pub(crate) fn track(&self, key: TrackKey) -> Result<&Track> {
        self.presentation().track(key)
    }

    /// The read-only view renderers work from.
    pub(crate) fn presentation(&self) -> Presentation<'_> {
        Presentation::new(&self.index.tracks, &self.plan, &self.version)
    }

    pub(crate) fn init_segment(&self, key: TrackKey) -> Result<Bytes> {
        self.init_segments
            .get(&key)
            .cloned()
            .ok_or(Error::NotFound("track does not exist"))
    }

    pub(crate) fn hls_master_playlist(&self) -> Bytes {
        self.rendered.hls_master.clone()
    }

    pub(crate) fn hls_media_playlist(&self, key: TrackKey) -> Result<Bytes> {
        self.rendered
            .hls_media
            .get(&key)
            .cloned()
            .ok_or(Error::NotFound("track does not exist"))
    }

    pub(crate) fn dash_manifest(&self) -> Bytes {
        self.rendered.dash.clone()
    }

    pub(crate) fn prepare_media_segment(
        &self,
        key: TrackKey,
        segment_index: u32,
    ) -> Result<fmp4::PreparedSegment> {
        let track = self.track(key)?;
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

    /// Logs what packaging left out or adjusted, so an operator can see why a file behaves as it
    /// does without inspecting it.
    pub(crate) fn log_load_details(&self, asset_id: &str) {
        if let Some(fragmentation) = self.index.fragmentation {
            tracing::info!(
                event = "fragments_discovered",
                asset.id = asset_id,
                media.fragments = fragmentation.fragments,
                discovery = ?fragmentation.discovery,
            );
            if let Some(dropped) = fragmentation.dropped_tail {
                tracing::warn!(
                    event = "truncated_tail_dropped",
                    asset.id = asset_id,
                    dropped.bytes = dropped.bytes,
                    dropped.fragments = dropped.fragments,
                );
            }
        }
        for skipped in &self.index.skipped_tracks {
            tracing::info!(
                event = "track_skipped",
                asset.id = asset_id,
                track.id = skipped.id,
                track.handler = %skipped.handler,
                reason = skipped.reason,
            );
        }
        for track in self
            .index
            .tracks
            .iter()
            .filter(|track| track.timeline_shift > 0)
        {
            tracing::info!(
                event = "edit_list_applied",
                asset.id = asset_id,
                track.id = track.id,
                track.key = %track.key,
                timeline_shift_ticks = track.timeline_shift,
                track.timescale = track.timescale,
            );
        }
    }

    /// Points a remote asset at a re-signed URL for the same object.
    pub(crate) fn update_location(&self, url: &reqwest::Url) {
        self.source.update_location(url);
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
                hls::media_playlist(presentation, track.key)
                    .map(|playlist| (track.key, Bytes::from(playlist)))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        Ok(Self {
            hls_master: Bytes::from(hls::master_playlist(presentation)?),
            hls_media,
            dash: Bytes::from(dash::manifest(presentation)?),
        })
    }
}

/// Bumped whenever the bytes or text served for an unchanged source change: the init segment
/// layout, the timeline mapping, or the playlist and manifest format.
///
/// Media URLs are cached as immutable by browsers and CDNs, so a new build that answers an old
/// URL with different bytes would be served stale content until the cache expires. Mixing the
/// revision into the version gives such a build new URLs instead.
const FORMAT_REVISION: u32 = 1;

/// The `v` value in media URLs: a hash of everything the index was built from (`moov`, and every
/// `moof` of a fragmented file) and [`FORMAT_REVISION`].
fn version_of(index: &MediaIndex) -> String {
    use std::fmt::Write;

    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(
        index
            .source
            .metadata_sha256
            .expect("parsed assets always have a metadata hash"),
    );
    hasher.update(FORMAT_REVISION.to_be_bytes());
    hasher
        .finalize()
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut version, byte| {
            write!(version, "{byte:02x}").expect("writing to a String cannot fail");
            version
        })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    #[tokio::test]
    async fn the_version_depends_on_the_format_revision_and_not_only_on_the_file() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/h264-aac.mp4");
        let asset = PackagedAsset::load_local(path, 1000, &LimitsConfig::default())
            .await
            .expect("fixture should load");

        // What the version would be if it were only the first bytes of the moov hash.
        let moov_only = asset.index.source.moov_sha256.unwrap().iter().take(8).fold(
            String::new(),
            |mut hex, byte| {
                write!(hex, "{byte:02x}").unwrap();
                hex
            },
        );

        assert_eq!(asset.version().len(), 16);
        assert!(asset.version().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(asset.version(), moov_only);
        assert_eq!(
            version_of(&asset.index),
            asset.version(),
            "and it is stable"
        );
    }

    #[tokio::test]
    async fn fragmented_files_with_the_same_moov_but_different_content_get_different_versions() {
        // The two files differ only in the `tfdt` values inside their `moof` boxes, so their
        // `moov` boxes are identical. A version taken from `moov` alone would be the same, and a
        // CDN would serve one file's immutable segments for the other.
        let load = |name: &'static str| async move {
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name);
            PackagedAsset::load_local(path, 1000, &LimitsConfig::default())
                .await
                .expect("fixture should load")
        };

        let plain = load("h264-aac-fragmented.mp4").await;
        let shifted = load("h264-aac-fragmented-offset.mp4").await;

        assert_eq!(
            plain.index.source.moov_sha256, shifted.index.source.moov_sha256,
            "the premise: the moov boxes are the same"
        );
        assert_ne!(plain.version(), shifted.version());
        let again = load("h264-aac-fragmented.mp4").await;
        assert_eq!(plain.version(), again.version(), "and a version is stable");
    }

    /// The plain fragmented fixture with its last 20 kB removed: inside the final fragment.
    fn cut_copy(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let whole = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/h264-aac-fragmented.mp4");
        let directory =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/asset-tests");
        std::fs::create_dir_all(&directory).unwrap();
        let cut = directory.join(name);
        let bytes = std::fs::read(&whole).unwrap();
        std::fs::write(&cut, &bytes[..bytes.len() - 20_000]).unwrap();
        (whole, cut)
    }

    #[tokio::test]
    async fn a_cut_fragmented_file_is_refused_by_default_and_served_short_when_tolerated() {
        let (whole, cut) = cut_copy("cut.mp4");
        let limits = LimitsConfig::default();

        let refused = PackagedAsset::load_local(&cut, 1000, &limits)
            .await
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("limits.tolerate_truncated_tail"),
            "{refused}"
        );

        let tolerant = LimitsConfig {
            tolerate_truncated_tail: true,
            ..LimitsConfig::default()
        };
        let short = PackagedAsset::load_local(&cut, 1000, &tolerant)
            .await
            .unwrap();
        let full = PackagedAsset::load_local(&whole, 1000, &tolerant)
            .await
            .unwrap();

        assert_eq!(full.plan.segments.len(), 3);
        assert_eq!(
            short.plan.segments.len(),
            2,
            "the incomplete third fragment is left out"
        );
        let dropped = short
            .index
            .fragmentation
            .unwrap()
            .dropped_tail
            .expect("a tail was dropped");
        assert!(dropped.bytes > 0);
        assert!(full.index.fragmentation.unwrap().dropped_tail.is_none());
        // Different content, so a different URL: a CDN must not mix the two.
        assert_ne!(short.version(), full.version());
        // Every segment it does serve is fully inside the file.
        for segment in &short.plan.segments {
            for part in &segment.tracks {
                assert!(part.end_sample > part.first_sample);
            }
        }
    }
}
