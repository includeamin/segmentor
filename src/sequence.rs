//! Sequences: several clips played back to back as one stream (TDD 0008).
//!
//! Each clip is its own [`PackagedAsset`], built from a trimmed sample index exactly as a whole
//! file would be. This module checks the clips can share one stream, numbers their segments
//! across all of them, and renders the playlists and manifest that span them.

use std::collections::HashMap;
use std::mem::Discriminant;
use std::sync::Arc;

use bytes::Bytes;

use crate::asset::PackagedAsset;
use crate::clip::{self, ClipWindow, TimelinePosition};
use crate::composite::ServedAsset;
use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::fmp4::PreparedSegment;
use crate::media::{CodecConfig, MediaIndex, TrackKey};
use crate::mp4::ParsedMedia;
use crate::protocol::{SequenceClip, dash, hls};
use crate::source::MediaSourceKind;

/// One file the clips cut from, opened and parsed once however many clips use it.
pub(crate) struct ClipFile {
    pub(crate) source: MediaSourceKind,
    pub(crate) parsed: ParsedMedia,
}

#[derive(Debug)]
pub(crate) struct SequenceAsset {
    clips: Vec<Arc<PackagedAsset>>,
    /// The global number of each clip's first segment.
    first_segments: Vec<u32>,
    total: TimelinePosition,
    version: String,
    rendered: SequenceManifests,
}

#[derive(Debug)]
struct SequenceManifests {
    hls_master: Bytes,
    hls_media: HashMap<TrackKey, Bytes>,
    dash: Bytes,
}

/// Trims every clip from its already parsed file, in order, each starting where the one before
/// ended, and builds what is served (TDD 0008, "Loading"). One clip is an ordinary asset; two or
/// more are a sequence. Errors name the clip by its position in the mapper's list.
#[allow(
    dead_code,
    reason = "TEMPORARY: first called by the registry (plan Task 6)"
)]
pub(crate) fn build(
    asset_id: &str,
    files: &[ClipFile],
    clips: &[(usize, ClipWindow)],
    mapper_version: &str,
    segment_duration_ms: u64,
    limits: &LimitsConfig,
) -> Result<ServedAsset> {
    let named = |position: usize| {
        move |error: Error| Error::InvalidMedia(format!("clip {position}: {error}"))
    };
    let mut start = TimelinePosition::ZERO;
    let mut trimmed = Vec::with_capacity(clips.len());
    for (position, &(file, window)) in clips.iter().enumerate() {
        let cut = clip::trim(&files[file].parsed.index, window, start).map_err(named(position))?;
        tracing::info!(
            event = "clip_trimmed",
            asset.id = asset_id,
            clip = position,
            requested.from_ms = window.from_ms,
            requested.to_ms = ?window.to_ms,
            served.from_ms = cut.served_from_ms,
            served.to_ms = cut.served_to_ms,
        );
        trimmed.push((file, start, cut.index));
        start = cut.end;
    }
    let version = clip::version_of(
        mapper_version,
        clips
            .iter()
            .map(|&(file, window)| (&files[file].parsed.index.source, window)),
    );
    let mut assets = Vec::with_capacity(trimmed.len());
    for (position, (file, clip_start, index)) in trimmed.into_iter().enumerate() {
        let asset = PackagedAsset::assemble(
            files[file].source.clone(),
            index,
            &files[file].parsed.metadata,
            Vec::new(),
            segment_duration_ms,
            limits,
            Some(version.clone()),
        )
        .map_err(named(position))?;
        assets.push((asset, clip_start));
    }
    if assets.len() == 1 {
        let (asset, _) = assets.pop().expect("checked the length");
        return Ok(ServedAsset::Single(Arc::new(asset)));
    }
    Ok(ServedAsset::Sequence(Box::new(assemble(
        assets, start, version,
    )?)))
}

fn assemble(
    clips: Vec<(PackagedAsset, TimelinePosition)>,
    total: TimelinePosition,
    version: String,
) -> Result<SequenceAsset> {
    check_compatible(&clips)?;
    let mut first_segments = Vec::with_capacity(clips.len());
    let mut next = 0u32;
    for (asset, _) in &clips {
        first_segments.push(next);
        next = u32::try_from(asset.plan.segments.len())
            .ok()
            .and_then(|count| next.checked_add(count))
            .ok_or_else(|| Error::InvalidMedia("a sequence has too many segments".to_owned()))?;
    }
    let (clips, starts): (Vec<_>, Vec<_>) = clips
        .into_iter()
        .map(|(asset, start)| (Arc::new(asset), start))
        .unzip();
    let rendered = render(&clips, &first_segments, &starts, total, &version)?;
    Ok(SequenceAsset {
        clips,
        first_segments,
        total,
        version,
        rendered,
    })
}

/// A clip's tracks by key, each with its codec family and codec string, sorted by key so the
/// order tracks appear in their files does not matter.
fn layout(index: &MediaIndex) -> Vec<(String, Discriminant<CodecConfig>, String)> {
    let mut tracks = index
        .tracks
        .iter()
        .map(|track| {
            (
                track.key.to_string(),
                std::mem::discriminant(&track.codec),
                track.codec.codecs(),
            )
        })
        .collect::<Vec<_>>();
    tracks.sort_by(|a, b| a.0.cmp(&b.0));
    tracks
}

/// Every clip must have the same tracks, and the same codec in each (TDD 0008, "Compatibility
/// between clips"). Anything within a codec may differ: resolution, profile, parameter sets.
fn check_compatible(clips: &[(PackagedAsset, TimelinePosition)]) -> Result<()> {
    let Some(((first, _), rest)) = clips.split_first() else {
        return Err(Error::InvalidMedia(
            "a sequence needs at least one clip".to_owned(),
        ));
    };
    let expected = layout(&first.index);
    let names = |tracks: &[(String, Discriminant<CodecConfig>, String)]| {
        tracks
            .iter()
            .map(|(key, _, _)| key.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    for (offset, (clip, _)) in rest.iter().enumerate() {
        let position = offset + 1;
        let found = layout(&clip.index);
        if expected
            .iter()
            .map(|(key, _, _)| key)
            .ne(found.iter().map(|(key, _, _)| key))
        {
            return Err(Error::InvalidMedia(format!(
                "clips 0 and {position} have different tracks ({} vs {})",
                names(&expected),
                names(&found)
            )));
        }
        for ((key, family, codec), (_, other_family, other_codec)) in expected.iter().zip(&found) {
            if family != other_family {
                return Err(Error::InvalidMedia(format!(
                    "clips 0 and {position} use different codecs for track `{key}` ({codec} vs {other_codec})"
                )));
            }
        }
    }
    Ok(())
}

fn render(
    clips: &[Arc<PackagedAsset>],
    first_segments: &[u32],
    starts: &[TimelinePosition],
    total: TimelinePosition,
    version: &str,
) -> Result<SequenceManifests> {
    let views = clips
        .iter()
        .zip(first_segments)
        .zip(starts)
        .map(|((clip, &first_segment), start)| SequenceClip {
            presentation: clip.presentation(),
            first_segment,
            start_nanos: start.nanos(),
        })
        .collect::<Vec<_>>();
    let hls_media = views[0]
        .presentation
        .tracks()
        .iter()
        .map(|track| {
            hls::sequence_media_playlist(&views, track.key, version)
                .map(|playlist| (track.key, Bytes::from(playlist)))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    Ok(SequenceManifests {
        hls_master: Bytes::from(hls::sequence_master_playlist(&views, version)?),
        hls_media,
        dash: Bytes::from(dash::sequence_manifest(&views, total.nanos(), version)?),
    })
}

impl SequenceAsset {
    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    pub(crate) fn hls_master(&self) -> Bytes {
        self.rendered.hls_master.clone()
    }

    pub(crate) fn dash(&self) -> Bytes {
        self.rendered.dash.clone()
    }

    pub(crate) fn media_playlist(&self, key: TrackKey) -> Result<Bytes> {
        self.rendered
            .hls_media
            .get(&key)
            .cloned()
            .ok_or(Error::NotFound("track does not exist"))
    }

    #[allow(
        dead_code,
        reason = "TEMPORARY: first used by the clip init route (plan Task 7)"
    )]
    pub(crate) fn clip_init_segment(&self, key: TrackKey, clip: usize) -> Result<Bytes> {
        self.clips
            .get(clip)
            .ok_or(Error::NotFound("clip does not exist"))?
            .init_segment(key)
    }

    /// The fragment for global segment `segment_index`, and the clip whose bytes it reads.
    pub(crate) fn prepare_segment(
        &self,
        key: TrackKey,
        segment_index: u32,
    ) -> Result<(Arc<PackagedAsset>, PreparedSegment)> {
        // `first_segments[0]` is 0, so at least one clip starts at or before any index.
        let clip = self
            .first_segments
            .partition_point(|&first| first <= segment_index)
            - 1;
        let local = segment_index - self.first_segments[clip];
        let asset = &self.clips[clip];
        let prepared =
            asset.prepare_numbered_segment(key, local, segment_index.saturating_add(1))?;
        Ok((Arc::clone(asset), prepared))
    }

    /// Clips cut from the same file share one source, so rotating any of them rotates all.
    pub(crate) fn update_location(&self, clip: usize, url: &reqwest::Url) {
        if let Some(asset) = self.clips.get(clip) {
            asset.update_location(url);
        }
    }

    pub(crate) fn log_load_details(&self, asset_id: &str) {
        for (position, clip) in self.clips.iter().enumerate() {
            clip.log_load_details(&format!("{asset_id}/clip-{position}"));
        }
    }

    pub(crate) fn index_bytes(&self) -> u64 {
        let clips: u64 = self.clips.iter().map(|clip| clip.index_bytes()).sum();
        let rendered = self.rendered.hls_master.len()
            + self.rendered.dash.len()
            + self
                .rendered
                .hls_media
                .values()
                .map(Bytes::len)
                .sum::<usize>();
        clips.saturating_add(rendered as u64)
    }

    pub(crate) fn track_count(&self) -> usize {
        self.clips[0].presentation().tracks().len()
    }

    pub(crate) fn segment_count(&self) -> usize {
        self.clips.iter().map(|clip| clip.plan.segments.len()).sum()
    }

    #[allow(
        dead_code,
        reason = "TEMPORARY: first used by the clip init route (plan Task 7)"
    )]
    pub(crate) fn clip_count(&self) -> usize {
        self.clips.len()
    }

    /// Rounded through `u32` milliseconds first, like `composite::index_duration_seconds`, so the
    /// conversion never silently loses precision.
    pub(crate) fn duration_seconds(&self) -> f64 {
        f64::from(u32::try_from(self.total.millis()).unwrap_or(u32::MAX)) / 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::LocalMediaSource;
    use crate::testutil::{fixture, tfdt};

    async fn file(name: &str) -> ClipFile {
        let source =
            MediaSourceKind::Local(Arc::new(LocalMediaSource::open(fixture(name)).unwrap()));
        let parsed = crate::mp4::parse(&source, &LimitsConfig::default())
            .await
            .unwrap();
        ClipFile { source, parsed }
    }

    const WHOLE: ClipWindow = ClipWindow {
        from_ms: 0,
        to_ms: None,
    };

    fn build_from(files: &[ClipFile], clips: &[(usize, ClipWindow)]) -> Result<ServedAsset> {
        build("test", files, clips, "v1", 1000, &LimitsConfig::default())
    }

    fn sequence(asset: &ServedAsset) -> &SequenceAsset {
        match asset {
            ServedAsset::Sequence(sequence) => sequence,
            _ => panic!("expected a sequence"),
        }
    }

    #[tokio::test]
    async fn global_segment_numbers_find_their_clip() {
        let files = [file("h264-aac.mp4").await];
        let asset = build_from(&files, &[(0, WHOLE), (0, WHOLE)]).unwrap();
        let sequence = sequence(&asset);

        let (_, first) = sequence.prepare_segment(TrackKey::VIDEO, 0).unwrap();
        let (_, fourth) = sequence.prepare_segment(TrackKey::VIDEO, 3).unwrap();

        assert_eq!(
            fourth.ranges, first.ranges,
            "segment 3 is the second clip's first"
        );
        assert_eq!(
            &fourth.header[20..24],
            &4u32.to_be_bytes(),
            "numbered across the sequence"
        );
        assert!(matches!(
            sequence.prepare_segment(TrackKey::VIDEO, 6),
            Err(Error::NotFound(_))
        ));
        assert_eq!(asset.segment_count(), 6);
    }

    #[tokio::test]
    async fn timestamps_continue_across_a_clip_boundary() {
        let files = [file("h264-aac.mp4").await];
        let asset = build_from(&files, &[(0, WHOLE), (0, WHOLE)]).unwrap();
        let sequence = sequence(&asset);

        let (_, last_of_first) = sequence.prepare_segment(TrackKey::VIDEO, 2).unwrap();
        let (_, first_of_second) = sequence.prepare_segment(TrackKey::VIDEO, 3).unwrap();
        let (_, audio) = sequence.prepare_segment(TrackKey::audio(1), 3).unwrap();

        // 90 frames of 512 ticks: the first clip ends at 46080 (3 s), where the second begins.
        assert_eq!(tfdt(&last_of_first.header), 30_720);
        assert_eq!(tfdt(&first_of_second.header), 46_080);
        assert_eq!(tfdt(&audio.header), 144_000, "audio moves by the same 3 s");
    }

    #[tokio::test]
    async fn clips_of_different_resolutions_share_one_stream() {
        let files = [
            file("rendition-720p.mp4").await,
            file("rendition-480p.mp4").await,
        ];

        let asset = build_from(&files, &[(0, WHOLE), (1, WHOLE)]).unwrap();

        let master = String::from_utf8(asset.hls_master_playlist().to_vec()).unwrap();
        assert!(master.contains("RESOLUTION=640x360"), "{master}");
        let init = asset.clip_init_segment(None, TrackKey::VIDEO, 1).unwrap();
        assert_eq!(&init[4..8], b"ftyp");
    }

    #[tokio::test]
    async fn a_video_clip_then_an_audio_only_clip_is_refused_naming_both() {
        let files = [file("h264-aac.mp4").await, file("aac-only.m4a").await];

        let error = build_from(&files, &[(0, WHOLE), (1, WHOLE)]).unwrap_err();

        assert!(error.to_string().contains("clips 0 and 1"), "{error}");
    }

    #[tokio::test]
    async fn a_codec_change_within_a_track_is_refused_naming_the_track() {
        let files = [file("h264-aac.mp4").await, file("hevc-aac.mp4").await];

        let error = build_from(&files, &[(0, WHOLE), (1, WHOLE)]).unwrap_err();

        assert!(error.to_string().contains("clips 0 and 1"), "{error}");
        assert!(error.to_string().contains("`video`"), "{error}");
    }

    #[tokio::test]
    async fn the_layout_ignores_the_order_tracks_appear_in() {
        let parsed = file("h264-aac.mp4").await.parsed;
        let mut reordered = parsed.index.clone();
        reordered.tracks.reverse();

        assert_eq!(layout(&parsed.index), layout(&reordered));
    }

    #[tokio::test]
    async fn one_clip_is_served_as_an_ordinary_asset_under_its_own_version() {
        let files = [file("h264-aac.mp4").await];

        let asset = build_from(&files, &[(0, WHOLE)]).unwrap();

        assert!(matches!(asset, ServedAsset::Single(_)));
        let plain =
            PackagedAsset::load_local(fixture("h264-aac.mp4"), 1000, &LimitsConfig::default())
                .await
                .unwrap();
        assert_ne!(
            asset.version(),
            plain.version(),
            "a clip's window is part of its version"
        );
        assert!(asset.init_segment(None, TrackKey::VIDEO).is_ok());
    }

    #[tokio::test]
    async fn a_sequence_has_per_clip_init_segments_and_no_plain_one() {
        let files = [file("h264-aac.mp4").await];

        let asset = build_from(&files, &[(0, WHOLE), (0, WHOLE)]).unwrap();

        assert!(matches!(
            asset.init_segment(None, TrackKey::VIDEO),
            Err(Error::NotFound(_))
        ));
        assert_eq!(
            &asset.clip_init_segment(None, TrackKey::VIDEO, 1).unwrap()[4..8],
            b"ftyp"
        );
        assert!(matches!(
            asset.clip_init_segment(None, TrackKey::VIDEO, 2),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            asset.clip_init_segment(Some("720p"), TrackKey::VIDEO, 0),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            asset.hls_iframe_playlist(),
            Err(Error::NotFound(_))
        ));
        let media = String::from_utf8(
            asset
                .hls_media_playlist(None, TrackKey::VIDEO)
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            media.contains(&format!("clips/1/init.mp4?v={}", asset.version())),
            "{media}"
        );
        let manifest = String::from_utf8(asset.dash_manifest().to_vec()).unwrap();
        assert_eq!(manifest.matches("<Period ").count(), 2, "{manifest}");
    }
}
