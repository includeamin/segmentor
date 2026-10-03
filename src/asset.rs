use std::collections::HashMap;
use std::path::Path;

use bytes::Bytes;

use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{MediaIndex, Track, TrackKey, TrackKind};
use crate::mp4::ParsedMedia;
use crate::protocol::{Manifest, Presentation, dash, hls};
use crate::segment::{SegmentPlan, TrackSegment};
use crate::source::{ByteRange, LocalMediaSource, MediaSourceKind, Metadata};
use crate::subtitle::{self, Subtitle};
use crate::{fmp4, mp4, segment};
use std::sync::{Arc, OnceLock};

/// What a load adds to the file itself: sidecar subtitles, a URL version chosen by the caller (a
/// clip's), content encryption, and whether HLS carries audio inside the video segments.
#[derive(Debug, Default)]
pub(crate) struct Extras {
    pub(crate) subtitles: Vec<Subtitle>,
    pub(crate) version: Option<String>,
    pub(crate) encryption: Option<Arc<crate::cenc::Encryption>>,
    /// Asks for a muxed HLS stream (TDD 0011); honoured only when the asset can have one.
    pub(crate) hls_mux_audio: bool,
}

#[derive(Debug)]
pub(crate) struct PackagedAsset {
    source: MediaSourceKind,
    pub(crate) index: MediaIndex,
    pub(crate) plan: SegmentPlan,
    init_segments: HashMap<TrackKey, Bytes>,
    limits: LimitsConfig,
    version: String,
    rendered: RenderedManifests,
    subtitles: Vec<Subtitle>,
    protection: Option<crate::cenc::AssetProtection>,
    /// The HLS stream that carries the default audio inside the video segments, when the load
    /// asked for one and the asset can have it (TDD 0011).
    muxed: Option<Muxed>,
}

/// The muxed HLS stream: video plus the first audio track, in one init segment and one
/// fragment per segment.
#[derive(Debug)]
struct Muxed {
    video: TrackKey,
    audio: TrackKey,
    init: Bytes,
}

/// Playlists and manifests, each rendered at most once so requests never walk sample tables.
///
/// Only the master playlist is rendered when the asset loads: it is the first thing a player
/// asks for. Everything else is rendered on its first request and kept, so a cold load does not
/// pay for a DASH manifest an HLS player never fetches, and clips inside a sequence (which
/// serves its own playlists) never render theirs at all.
#[derive(Debug, Default)]
struct RenderedManifests {
    hls_master: Manifest,
    hls_media: HashMap<TrackKey, OnceLock<Manifest>>,
    hls_iframes: OnceLock<Option<Manifest>>,
    hls_subtitle: OnceLock<Manifest>,
    hls_muxed: OnceLock<Manifest>,
    dash: OnceLock<Manifest>,
}

/// The cached value, or `render`'s result cached on first success. Two first requests racing
/// may both render; the first to finish is kept and the other's identical copy is dropped.
fn cached<T: Clone>(cell: &OnceLock<T>, render: impl FnOnce() -> Result<T>) -> Result<T> {
    if let Some(value) = cell.get() {
        return Ok(value.clone());
    }
    let value = render()?;
    Ok(cell.get_or_init(|| value).clone())
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
        Self::load_with(source, Extras::default(), segment_duration_ms, limits).await
    }

    /// Like [`Self::load`], with sidecar subtitle files already fetched and, optionally, content
    /// encryption. Each subtitle is validated and moved onto the asset's timeline; one bad file
    /// fails the whole asset.
    pub(crate) async fn load_with(
        source: MediaSourceKind,
        extras: Extras,
        segment_duration_ms: u64,
        limits: &LimitsConfig,
    ) -> Result<Self> {
        let parsed = mp4::parse(&source, limits).await?;
        let limits = limits.clone();
        tokio::task::spawn_blocking(move || {
            let ParsedMedia { index, metadata } = parsed;
            Self::assemble(
                source,
                index,
                &metadata,
                segment_duration_ms,
                &limits,
                extras,
            )
        })
        .await
        .map_err(|error| Error::Io(std::io::Error::other(error)))?
    }

    /// Plans, writes init segments, and renders playlists for an already parsed index: a whole
    /// file's, or one trimmed to a clip (see `clip::trim`). `extras.version` replaces the URL
    /// version derived from the content, for a clip whose version must also cover its window.
    pub(crate) fn assemble(
        source: MediaSourceKind,
        index: MediaIndex,
        metadata: &Metadata,
        segment_duration_ms: u64,
        limits: &LimitsConfig,
        extras: Extras,
    ) -> Result<Self> {
        let Extras {
            subtitles,
            version,
            encryption,
            hls_mux_audio,
        } = extras;
        let plan = segment::plan(&index, segment_duration_ms, limits)?;
        let subtitles = prepare_subtitles(&index, subtitles)?;
        let protection = encryption
            .map(|encryption| {
                crate::cenc::AssetProtection::new(encryption, &index.tracks, metadata)
            })
            .transpose()?;
        let init_segments = index
            .tracks
            .iter()
            .map(|track| {
                let bytes = match protection
                    .as_ref()
                    .and_then(|p| p.track(track.id).map(|t| (p, t)))
                {
                    Some((asset, track_protection)) => fmp4::write_protected_init_segment(
                        metadata,
                        track.id,
                        &track_protection.init(&asset.pssh),
                    ),
                    None => fmp4::write_init_segment(metadata, track.id),
                }?;
                Ok((track.key, Bytes::from(bytes)))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        // Muxing needs a video and an audio track, and is not offered for encrypted content
        // yet: a fragment would need encryption boxes per track (TDD 0011).
        let muxed = if hls_mux_audio && protection.is_none() {
            let video = index
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Video);
            let audio = index
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Audio);
            video
                .zip(audio)
                .map(|(video, audio)| {
                    Ok::<_, Error>(Muxed {
                        video: video.key,
                        audio: audio.key,
                        init: Bytes::from(fmp4::write_muxed_init_segment(
                            metadata,
                            &[video.id, audio.id],
                        )?),
                    })
                })
                .transpose()?
        } else {
            None
        };
        let encryption_ref = protection.as_ref().map(|p| p.encryption.as_ref());
        let version = version.unwrap_or_else(|| version_of(&index, &subtitles, encryption_ref));
        let rendered = RenderedManifests::render(
            Presentation::new(&index.tracks, &plan, &version)
                .with_subtitles(&subtitles)
                .with_encryption(encryption_ref)
                .with_muxed_audio(muxed.is_some()),
        )?;
        Ok(Self {
            source,
            index,
            plan,
            init_segments,
            limits: limits.clone(),
            version,
            rendered,
            subtitles,
            protection,
            muxed,
        })
    }

    pub(crate) fn track(&self, key: TrackKey) -> Result<&Track> {
        self.presentation().track(key)
    }

    /// The read-only view renderers work from.
    pub(crate) fn presentation(&self) -> Presentation<'_> {
        Presentation::new(&self.index.tracks, &self.plan, &self.version)
            .with_subtitles(&self.subtitles)
            .with_encryption(self.encryption())
            .with_muxed_audio(self.muxed.is_some())
    }

    /// Whether HLS serves the muxed stream (TDD 0011).
    pub(crate) const fn is_muxed(&self) -> bool {
        self.muxed.is_some()
    }

    fn muxed(&self) -> Result<&Muxed> {
        self.muxed
            .as_ref()
            .ok_or(Error::NotFound("track does not exist"))
    }

    /// The muxed stream's init segment: the video and audio tracks in one `moov`.
    pub(crate) fn muxed_init_segment(&self) -> Result<Bytes> {
        Ok(self.muxed()?.init.clone())
    }

    /// The muxed stream's media playlist. Its segments are the video's, with the same
    /// durations, and its URIs are relative, so the text is the video playlist's.
    pub(crate) fn hls_muxed_playlist(&self) -> Result<Manifest> {
        let muxed = self.muxed()?;
        cached(&self.rendered.hls_muxed, || {
            hls::media_playlist(self.presentation(), muxed.video).map(Manifest::from)
        })
    }

    /// Segment `segment_index` of the muxed stream: the planned video and audio samples of that
    /// segment in one fragment.
    pub(crate) fn prepare_muxed_segment(
        &self,
        segment_index: u32,
    ) -> Result<fmp4::PreparedSegment> {
        let muxed = self.muxed()?;
        let segment = self
            .plan
            .segments
            .get(usize::try_from(segment_index).map_err(|_| {
                Error::InvalidMedia("segment index does not fit in memory".to_owned())
            })?)
            .ok_or(Error::NotFound("segment does not exist"))?;
        let parts = [muxed.video, muxed.audio]
            .into_iter()
            .map(|key| {
                let track = self.track(key)?;
                let part = segment
                    .tracks
                    .iter()
                    .find(|candidate| candidate.track_id == track.id)
                    .copied()
                    .ok_or_else(|| Error::InvalidMedia("segment is missing a track".to_owned()))?;
                Ok((track, part))
            })
            .collect::<Result<Vec<_>>>()?;
        fmp4::prepare_muxed_segment(&parts, segment_index.saturating_add(1), &self.limits)
    }

    /// The content keys and DRM systems this asset is encrypted with, if any.
    pub(crate) fn encryption(&self) -> Option<&crate::cenc::Encryption> {
        self.protection
            .as_ref()
            .map(|protection| protection.encryption.as_ref())
    }

    /// Marks an encrypted track's segment as still to be encrypted once its bytes are read.
    fn with_encryption(
        &self,
        track: &Track,
        segment: TrackSegment,
        sequence_number: u32,
        mut prepared: fmp4::PreparedSegment,
    ) -> fmp4::PreparedSegment {
        if let Some(protection) = self.protection.as_ref().and_then(|p| p.track(track.id)) {
            prepared.encryption = Some(Box::new(crate::cenc::PendingEncryption {
                track_id: track.id,
                kind: track.kind,
                samples: track
                    .samples
                    .to_vec(segment.first_sample..segment.end_sample),
                decode_time: segment.decode_time,
                sequence_number,
                protection: Arc::clone(protection),
            }));
        }
        prepared
    }

    pub(crate) fn init_segment(&self, key: TrackKey) -> Result<Bytes> {
        self.init_segments
            .get(&key)
            .cloned()
            .ok_or(Error::NotFound("track does not exist"))
    }

    pub(crate) fn hls_master_playlist(&self) -> Manifest {
        self.rendered.hls_master.clone()
    }

    pub(crate) fn hls_media_playlist(&self, key: TrackKey) -> Result<Manifest> {
        let cell = self
            .rendered
            .hls_media
            .get(&key)
            .ok_or(Error::NotFound("track does not exist"))?;
        cached(cell, || {
            hls::media_playlist(self.presentation(), key).map(Manifest::from)
        })
    }

    /// The video track's I-frame playlist, absent for an audio-only asset.
    pub(crate) fn hls_iframe_playlist(&self) -> Result<Manifest> {
        cached(&self.rendered.hls_iframes, || {
            Ok(hls::iframe_playlist(self.presentation())?.map(Manifest::from))
        })?
        .ok_or(Error::NotFound("asset has no video track"))
    }

    /// The fragment holding only the `frame_index`th keyframe of the video track.
    pub(crate) fn prepare_iframe(&self, frame_index: u32) -> Result<fmp4::PreparedSegment> {
        let presentation = self.presentation();
        let track = presentation
            .video()
            .ok_or(Error::NotFound("asset has no video track"))?;
        let position = track
            .samples
            .nth_sync(usize::try_from(frame_index).map_err(|_| {
                Error::InvalidMedia("keyframe index does not fit in memory".to_owned())
            })?)
            .ok_or(Error::NotFound("keyframe does not exist"))?;
        let sample = track.samples.get(position);
        let segment = TrackSegment {
            track_id: track.id,
            first_sample: position,
            end_sample: position + 1,
            decode_time: sample.decode_time,
            duration: u64::from(sample.duration),
        };
        let sequence_number = frame_index
            .checked_add(1)
            .ok_or_else(|| Error::InvalidMedia("sequence number overflow".to_owned()))?;
        let prepared = fmp4::prepare_media_segment(track, segment, sequence_number, &self.limits)?;
        Ok(self.with_encryption(track, segment, sequence_number, prepared))
    }

    /// The HLS playlist that lists one subtitle file.
    pub(crate) fn hls_subtitle_playlist(&self, language: &str) -> Result<Manifest> {
        self.subtitle(language)?;
        cached(&self.rendered.hls_subtitle, || {
            Ok(Manifest::from(hls::subtitle_playlist(self.presentation())))
        })
    }

    /// How many sidecar subtitle files this asset has, for status reporting.
    pub(crate) fn subtitle_count(&self) -> usize {
        self.subtitles.len()
    }

    /// A subtitle file, ready to serve.
    pub(crate) fn subtitle(&self, language: &str) -> Result<Bytes> {
        self.subtitles
            .iter()
            .find(|subtitle| subtitle.language.eq_ignore_ascii_case(language))
            .map(|subtitle| subtitle.data.clone())
            .ok_or(Error::NotFound("subtitle does not exist"))
    }

    pub(crate) fn dash_manifest(&self) -> Result<Manifest> {
        cached(&self.rendered.dash, || {
            dash::manifest(self.presentation()).map(Manifest::from)
        })
    }

    pub(crate) fn prepare_media_segment(
        &self,
        key: TrackKey,
        segment_index: u32,
    ) -> Result<fmp4::PreparedSegment> {
        // The segment is looked up first, so an out-of-range index is still a 404; no plan has
        // `u32::MAX` segments, so the saturation is never observed.
        self.prepare_numbered_segment(key, segment_index, segment_index.saturating_add(1))
    }

    /// Like [`Self::prepare_media_segment`], with the fragment's `mfhd` sequence number chosen by
    /// the caller: a sequence of clips numbers its fragments across every clip, not within one.
    pub(crate) fn prepare_numbered_segment(
        &self,
        key: TrackKey,
        segment_index: u32,
        sequence_number: u32,
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

        let prepared =
            fmp4::prepare_media_segment(track, track_segment, sequence_number, &self.limits)?;
        Ok(self.with_encryption(track, track_segment, sequence_number, prepared))
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
            .map(|track| track.samples.table_bytes())
            .sum::<usize>();
        let init = self.init_segments.values().map(Bytes::len).sum::<usize>()
            + self.muxed.as_ref().map_or(0, |muxed| muxed.init.len());
        let rendered = self.rendered.len();
        (samples as u64)
            .saturating_add(init as u64)
            .saturating_add(rendered as u64)
            .saturating_add(
                self.subtitles
                    .iter()
                    .map(|subtitle| subtitle.data.len() as u64)
                    .sum(),
            )
    }
}

impl RenderedManifests {
    fn render(presentation: Presentation<'_>) -> Result<Self> {
        Ok(Self {
            hls_master: Manifest::from(hls::master_playlist(presentation)?),
            hls_media: presentation
                .tracks()
                .iter()
                .map(|track| (track.key, OnceLock::new()))
                .collect(),
            ..Self::default()
        })
    }

    /// Bytes held by what has been rendered so far, compressed forms included.
    fn len(&self) -> usize {
        let rendered = |cell: &OnceLock<Manifest>| cell.get().map_or(0, Manifest::len);
        self.hls_master.len()
            + rendered(&self.dash)
            + rendered(&self.hls_subtitle)
            + rendered(&self.hls_muxed)
            + self.hls_media.values().map(rendered).sum::<usize>()
            + self
                .hls_iframes
                .get()
                .and_then(Option::as_ref)
                .map_or(0, Manifest::len)
    }
}

/// Bumped whenever the bytes or text served for an unchanged source change: the init segment
/// layout, the timeline mapping, or the playlist and manifest format.
///
/// Media URLs are cached as immutable by browsers and CDNs, so a new build that answers an old
/// URL with different bytes would be served stale content until the cache expires. Mixing the
/// revision into the version gives such a build new URLs instead.
///
/// The content hash under every version changed from SHA-256 to BLAKE3, for cold-start speed,
/// without a bump here: that change alone already gives every asset new URLs.
pub(crate) const FORMAT_REVISION: u32 = 2;

/// Validates each subtitle file and moves its cues onto the asset's timeline, by the offset the
/// edit lists were resolved with. A track's own delay is not part of it: that is already in the
/// presentation the cues were written against.
fn prepare_subtitles(index: &MediaIndex, subtitles: Vec<Subtitle>) -> Result<Vec<Subtitle>> {
    let offset_ms = index.presentation_offset_ms;
    subtitles
        .into_iter()
        .map(|mut subtitle| {
            subtitle.data = subtitle::prepare(&subtitle.language, &subtitle.data, offset_ms)?;
            Ok(subtitle)
        })
        .collect()
}

/// The `v` value in media URLs: a hash of everything the index was built from (`moov`, and every
/// `moof` of a fragmented file), [`FORMAT_REVISION`], and any subtitle files, so changing a
/// caption gives new URLs.
fn version_of(
    index: &MediaIndex,
    subtitles: &[Subtitle],
    encryption: Option<&crate::cenc::Encryption>,
) -> String {
    use std::fmt::Write;

    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(
        index
            .source
            .metadata_hash
            .expect("parsed assets always have a metadata hash"),
    );
    hasher.update(FORMAT_REVISION.to_be_bytes());
    // Absent subtitles add nothing, so an asset without them keeps the version it always had.
    for subtitle in subtitles {
        for part in [
            subtitle.language.as_bytes(),
            subtitle.label.as_bytes(),
            &[u8::from(subtitle.default), u8::from(subtitle.forced)],
            &subtitle.data,
        ] {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
    }
    // Nothing is added when clear, so a clear asset keeps the version it always had.
    if let Some(encryption) = encryption {
        hasher.update(b"cbcs");
        hasher.update(encryption.fingerprint());
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
        let moov_only = asset.index.source.moov_hash.unwrap().iter().take(8).fold(
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
            version_of(&asset.index, &[], None),
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
            plain.index.source.moov_hash, shifted.index.source.moov_hash,
            "the premise: the moov boxes are the same"
        );
        assert_ne!(plain.version(), shifted.version());
        let again = load("h264-aac-fragmented.mp4").await;
        assert_eq!(plain.version(), again.version(), "and a version is stable");
    }

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    async fn load_muxed(name: &str, encrypted: bool) -> PackagedAsset {
        let source = MediaSourceKind::Local(Arc::new(
            LocalMediaSource::open(fixture_path(name)).unwrap(),
        ));
        PackagedAsset::load_with(
            source,
            Extras {
                encryption: encrypted
                    .then(|| Arc::new(crate::cenc::tests_support::sample_encryption())),
                hls_mux_audio: true,
                ..Extras::default()
            },
            1000,
            &LimitsConfig::default(),
        )
        .await
        .unwrap()
    }

    /// TDD 0011: muxing needs video and audio, and is not offered for encrypted content yet;
    /// those assets keep separate renditions, and the muxed URLs are not there.
    #[tokio::test]
    async fn only_clear_assets_with_video_and_audio_are_muxed() {
        assert!(load_muxed("h264-aac.mp4", false).await.is_muxed());
        for (name, encrypted) in [
            ("h264-aac.mp4", true),
            ("aac-only.m4a", false),
            ("h264-video-only.mp4", false),
        ] {
            let asset = load_muxed(name, encrypted).await;
            assert!(!asset.is_muxed(), "{name}");
            assert!(asset.hls_muxed_playlist().is_err(), "{name}");
            assert!(asset.muxed_init_segment().is_err(), "{name}");
            assert!(asset.prepare_muxed_segment(0).is_err(), "{name}");
            let master = String::from_utf8(asset.hls_master_playlist().to_vec()).unwrap();
            assert!(!master.contains("muxed"), "{name}: {master}");
        }
    }

    #[tokio::test]
    async fn an_explicit_version_replaces_the_content_version_everywhere_it_is_served() {
        let source = MediaSourceKind::Local(Arc::new(
            LocalMediaSource::open(fixture_path("h264-aac.mp4")).unwrap(),
        ));
        let limits = LimitsConfig::default();
        let ParsedMedia { index, metadata } = mp4::parse(&source, &limits).await.unwrap();

        let asset = PackagedAsset::assemble(
            source,
            index,
            &metadata,
            1000,
            &limits,
            Extras {
                version: Some("feedfacefeedface".to_owned()),
                ..Extras::default()
            },
        )
        .unwrap();

        assert_eq!(asset.version(), "feedfacefeedface");
        let master = String::from_utf8(asset.hls_master_playlist().to_vec()).unwrap();
        assert!(master.contains("index.m3u8?v=feedfacefeedface"), "{master}");
        let media =
            String::from_utf8(asset.hls_media_playlist(TrackKey::VIDEO).unwrap().to_vec()).unwrap();
        assert!(media.contains("init.mp4?v=feedfacefeedface"), "{media}");
    }

    #[tokio::test]
    async fn a_caller_chosen_sequence_number_lands_in_the_fragment_header() {
        let asset =
            PackagedAsset::load_local(fixture_path("h264-aac.mp4"), 1000, &LimitsConfig::default())
                .await
                .unwrap();

        let plain = asset.prepare_media_segment(TrackKey::VIDEO, 1).unwrap();
        let numbered = asset
            .prepare_numbered_segment(TrackKey::VIDEO, 1, 42)
            .unwrap();

        // moof header (8 bytes), mfhd header (8), version and flags (4), then the number.
        assert_eq!(&plain.header[20..24], &2u32.to_be_bytes());
        assert_eq!(&numbered.header[20..24], &42u32.to_be_bytes());
        assert_eq!(plain.ranges, numbered.ranges);
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
