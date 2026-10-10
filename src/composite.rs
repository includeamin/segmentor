//! Adaptive assets: several files, each loaded exactly as a plain asset would be, served as one
//! title with several video qualities and a shared audio group.
//!
//! See `docs/technical-design/0006-trick-play-subtitles-and-renditions.md` (§3, Adaptive
//! renditions). Each rendition is its own [`PackagedAsset`], parsed, planned, and cached exactly
//! like today's single-file asset; nothing here changes that path. This module only decides which
//! rendition answers which URL, checks that the video renditions can be switched between, and
//! renders the master playlist and manifest that name them all.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;

use crate::asset::PackagedAsset;
use crate::error::{Error, Result};
use crate::fmp4;
use crate::media::{Track, TrackKey, TrackKind};
use crate::protocol::{AdaptiveAudio, AdaptiveVideo, Bandwidth, Manifest, Presentation, dash, hls};
use crate::resolver::LocationKey;
use crate::sequence::SequenceAsset;
use crate::subtitle::Subtitle;

/// What `state.asset()` serves: one file, several renditions served as one title, or several
/// clips played back to back (TDD 0008).
#[derive(Debug)]
pub(crate) enum ServedAsset {
    Single(Arc<PackagedAsset>),
    Composite(Box<CompositeAsset>),
    Sequence(Box<SequenceAsset>),
}

impl ServedAsset {
    /// The key IDs (as UUIDs) the asset is encrypted with, or `None` when it is served clear.
    pub(crate) fn key_ids(&self) -> Option<Vec<String>> {
        match self {
            Self::Single(asset) => asset.encryption().map(crate::cenc::Encryption::key_ids),
            Self::Composite(asset) => asset.video[0]
                .asset
                .encryption()
                .map(crate::cenc::Encryption::key_ids),
            Self::Sequence(asset) => asset.key_ids(),
        }
    }

    pub(crate) fn version(&self) -> &str {
        match self {
            Self::Single(asset) => asset.version(),
            Self::Composite(asset) => &asset.version,
            Self::Sequence(asset) => asset.version(),
        }
    }

    pub(crate) fn hls_master_playlist(&self) -> Manifest {
        match self {
            Self::Single(asset) => asset.hls_master_playlist(),
            Self::Composite(asset) => asset.rendered.hls_master.clone(),
            Self::Sequence(asset) => asset.hls_master(),
        }
    }

    pub(crate) fn dash_manifest(&self) -> Result<Manifest> {
        if self.is_hls_only() {
            return Err(Error::NotFound("an AES-128 asset has no DASH manifest"));
        }
        match self {
            Self::Single(asset) => asset.dash_manifest(),
            Self::Composite(asset) => Ok(asset.rendered.dash.clone()),
            Self::Sequence(asset) => Ok(asset.dash()),
        }
    }

    pub(crate) fn hls_iframe_playlist(&self) -> Result<Manifest> {
        match self {
            Self::Single(asset) => asset.hls_iframe_playlist(),
            Self::Composite(asset) => asset
                .rendered
                .hls_iframes
                .clone()
                .ok_or(Error::NotFound("asset has no video track")),
            Self::Sequence(_) => Err(Error::NotFound("a sequence has no I-frame playlist")),
        }
    }

    /// The prepared fragment, plus the specific underlying asset to stream its bytes from (the
    /// composite as a whole holds no readable source of its own).
    pub(crate) fn prepare_iframe(
        &self,
        frame_index: u32,
    ) -> Result<(Arc<PackagedAsset>, fmp4::PreparedSegment)> {
        match self {
            Self::Single(asset) => Ok((Arc::clone(asset), asset.prepare_iframe(frame_index)?)),
            Self::Composite(asset) => {
                let source = Arc::clone(&asset.video[asset.iframe_source].asset);
                let prepared = source.prepare_iframe(frame_index)?;
                Ok((source, prepared))
            }
            Self::Sequence(_) => Err(Error::NotFound("a sequence has no I-frame playlist")),
        }
    }

    pub(crate) fn hls_subtitle_playlist(&self, language: &str) -> Result<Manifest> {
        match self {
            Self::Single(asset) => asset.hls_subtitle_playlist(language),
            Self::Composite(asset) => {
                asset.subtitle(language)?;
                Ok(asset.rendered.hls_subtitle.clone())
            }
            Self::Sequence(_) => Err(Error::NotFound("subtitle does not exist")),
        }
    }

    pub(crate) fn subtitle(&self, language: &str) -> Result<Bytes> {
        match self {
            Self::Single(asset) => asset.subtitle(language),
            Self::Composite(asset) => asset.subtitle(language),
            Self::Sequence(_) => Err(Error::NotFound("subtitle does not exist")),
        }
    }

    /// `rendition` names which video rendition a `video-{id}` URL asked for; `None` for a plain
    /// asset's `video`, and for the shared `audio-{n}` group, which is never rendition-scoped.
    pub(crate) fn init_segment(&self, rendition: Option<&str>, key: TrackKey) -> Result<Bytes> {
        match self {
            Self::Single(asset) => {
                if rendition.is_some() {
                    return Err(Error::NotFound("track does not exist"));
                }
                asset.init_segment(key)
            }
            Self::Composite(asset) => asset.resolve(rendition, key)?.0.init_segment(key),
            Self::Sequence(_) => Err(Error::NotFound("a sequence's init segments are per clip")),
        }
    }

    /// One clip's init segment, served at `{track}/clips/{clip}/init.mp4`. Only a sequence has
    /// them; its clips may be encoded differently, so each has its own (TDD 0008, "URLs").
    pub(crate) fn clip_init_segment(
        &self,
        rendition: Option<&str>,
        key: TrackKey,
        clip: usize,
    ) -> Result<Bytes> {
        match self {
            Self::Sequence(asset) if rendition.is_none() => asset.clip_init_segment(key, clip),
            _ => Err(Error::NotFound("track does not exist")),
        }
    }

    pub(crate) fn hls_media_playlist(
        &self,
        rendition: Option<&str>,
        key: TrackKey,
    ) -> Result<Manifest> {
        match self {
            Self::Single(asset) => {
                if rendition.is_some() {
                    return Err(Error::NotFound("track does not exist"));
                }
                asset.hls_media_playlist(key)
            }
            Self::Composite(asset) => asset.media_playlist(rendition, key),
            Self::Sequence(asset) => {
                if rendition.is_some() {
                    return Err(Error::NotFound("track does not exist"));
                }
                asset.media_playlist(key)
            }
        }
    }

    /// The muxed HLS stream's playlist (TDD 0011); only a single-file asset can have one.
    pub(crate) fn hls_muxed_playlist(&self) -> Result<Manifest> {
        match self {
            Self::Single(asset) => asset.hls_muxed_playlist(),
            _ => Err(Error::NotFound("track does not exist")),
        }
    }

    pub(crate) fn muxed_init_segment(&self) -> Result<Bytes> {
        match self {
            Self::Single(asset) => asset.muxed_init_segment(),
            _ => Err(Error::NotFound("track does not exist")),
        }
    }

    pub(crate) fn prepare_muxed_segment(
        &self,
        segment_index: u32,
    ) -> Result<(Arc<PackagedAsset>, fmp4::PreparedSegment)> {
        match self {
            Self::Single(asset) => Ok((
                Arc::clone(asset),
                asset.prepare_muxed_segment(segment_index)?,
            )),
            _ => Err(Error::NotFound("track does not exist")),
        }
    }

    /// Whether the asset exists for HLS only: DASH has no whole-segment `AES-128` (TDD 0012).
    pub(crate) fn is_hls_only(&self) -> bool {
        match self {
            Self::Single(asset) => asset.is_aes128(),
            Self::Composite(asset) => asset.video[0].asset.is_aes128(),
            Self::Sequence(asset) => asset.is_aes128(),
        }
    }

    /// Whether HLS serves the muxed stream, for status reporting.
    pub(crate) fn is_muxed(&self) -> bool {
        matches!(self, Self::Single(asset) if asset.is_muxed())
    }

    /// See [`Self::prepare_iframe`] on why the underlying asset comes back too.
    pub(crate) fn prepare_media_segment(
        &self,
        rendition: Option<&str>,
        key: TrackKey,
        segment_index: u32,
    ) -> Result<(Arc<PackagedAsset>, fmp4::PreparedSegment)> {
        match self {
            Self::Single(asset) => {
                if rendition.is_some() {
                    return Err(Error::NotFound("track does not exist"));
                }
                Ok((
                    Arc::clone(asset),
                    asset.prepare_media_segment(key, segment_index)?,
                ))
            }
            Self::Composite(asset) => {
                let (source, key) = asset.resolve(rendition, key)?;
                let prepared = source.prepare_media_segment(key, segment_index)?;
                Ok((source, prepared))
            }
            Self::Sequence(asset) => {
                if rendition.is_some() {
                    return Err(Error::NotFound("track does not exist"));
                }
                asset.prepare_segment(key, segment_index)
            }
        }
    }

    /// Points a remote location at a re-signed URL for the same object. The shared audio group
    /// is never rotated this way because it is never itself the `location` a mapper answer names.
    pub(crate) fn update_location(&self, key: &LocationKey, url: &reqwest::Url) {
        match (self, key) {
            // A one-clip answer is served as `Single`.
            (Self::Single(asset), LocationKey::Main | LocationKey::Clip(0)) => {
                asset.update_location(url);
            }
            (Self::Sequence(asset), LocationKey::Clip(position)) => {
                asset.update_location(*position, url);
            }
            (Self::Composite(asset), LocationKey::Rendition(id)) => {
                if let Some(entry) = asset.video.iter().find(|entry| &entry.id == id) {
                    entry.asset.update_location(url);
                } else if let Some(entry) = asset.audio_only.iter().find(|entry| &entry.id == id) {
                    entry.asset.update_location(url);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn log_load_details(&self, asset_id: &str) {
        match self {
            Self::Single(asset) => asset.log_load_details(asset_id),
            Self::Composite(asset) => {
                for entry in &asset.video {
                    entry
                        .asset
                        .log_load_details(&format!("{asset_id}/video-{}", entry.id));
                }
                for entry in &asset.audio_only {
                    entry
                        .asset
                        .log_load_details(&format!("{asset_id}/{}", entry.id));
                }
            }
            Self::Sequence(asset) => asset.log_load_details(asset_id),
        }
    }

    pub(crate) fn index_bytes(&self) -> u64 {
        match self {
            Self::Single(asset) => asset.index_bytes(),
            Self::Composite(asset) => {
                let renditions: u64 = asset
                    .video
                    .iter()
                    .map(|entry| entry.asset.index_bytes())
                    .chain(
                        asset
                            .audio_only
                            .iter()
                            .map(|entry| entry.asset.index_bytes()),
                    )
                    .sum();
                let rendered = asset.rendered.hls_master.len()
                    + asset.rendered.dash.len()
                    + asset.rendered.hls_subtitle.len()
                    + asset.rendered.hls_iframes.as_ref().map_or(0, Manifest::len);
                renditions
                    .saturating_add(rendered as u64)
                    .saturating_add(asset.subtitles.iter().map(|s| s.data.len() as u64).sum())
            }
            Self::Sequence(asset) => asset.index_bytes(),
        }
    }

    /// Track count, for status reporting: every video rendition plus the shared audio group.
    pub(crate) fn track_count(&self) -> usize {
        match self {
            Self::Single(asset) => asset.presentation().tracks().len(),
            Self::Composite(asset) => asset.video.len() + asset.audio.len(),
            Self::Sequence(asset) => asset.track_count(),
        }
    }

    /// Segment count, for the load-completion log line. A composite's renditions are aligned to
    /// the same count (see `assemble`), so any one of them answers for the whole asset.
    pub(crate) fn segment_count(&self) -> usize {
        match self {
            Self::Single(asset) => asset.plan.segments.len(),
            Self::Composite(asset) => asset
                .video
                .first()
                .map_or(0, |entry| entry.asset.plan.segments.len()),
            Self::Sequence(asset) => asset.segment_count(),
        }
    }

    /// How many clips the asset plays, for status reporting: one for anything but a sequence.
    pub(crate) fn clip_count(&self) -> usize {
        match self {
            Self::Single(_) | Self::Composite(_) => 1,
            Self::Sequence(asset) => asset.clip_count(),
        }
    }

    pub(crate) fn subtitle_count(&self) -> usize {
        match self {
            Self::Single(asset) => asset.subtitle_count(),
            Self::Composite(asset) => asset.subtitles.len(),
            Self::Sequence(_) => 0,
        }
    }

    /// The longest track's duration, in seconds. Every video rendition of a composite is aligned
    /// to the same length (see `assemble`), so any one of them answers for the whole asset.
    pub(crate) fn duration_seconds(&self) -> f64 {
        match self {
            Self::Single(asset) => index_duration_seconds(&asset.index),
            Self::Composite(asset) => asset
                .video
                .first()
                .map_or(0.0, |entry| index_duration_seconds(&entry.asset.index)),
            Self::Sequence(asset) => asset.duration_seconds(),
        }
    }
}

/// An index's duration in seconds, as an `f64` for status reporting: rounded through `u32` first,
/// so the cast never silently loses precision the way a direct `u64 as f64` could.
fn index_duration_seconds(index: &crate::media::MediaIndex) -> f64 {
    if index.movie_timescale == 0 {
        0.0
    } else {
        f64::from(u32::try_from(index.duration.min(u64::from(u32::MAX))).unwrap_or(u32::MAX))
            / f64::from(index.movie_timescale)
    }
}

/// One video rendition, holding its own fully-loaded, independently cacheable asset.
#[derive(Debug)]
struct VideoEntry {
    id: String,
    asset: Arc<PackagedAsset>,
}

/// One audio-only rendition, contributing one or more tracks to the shared audio group.
#[derive(Debug)]
struct AudioOnlyEntry {
    id: String,
    asset: Arc<PackagedAsset>,
}

/// Where one member of the external `audio-{n}` group actually lives.
#[derive(Debug, Clone, Copy)]
enum AudioOwner {
    Video(usize),
    Dedicated(usize),
}

#[derive(Debug)]
struct AudioEntry {
    owner: AudioOwner,
    /// That owner's own internal key for this track (its own `audio-{m}`, not renumbered).
    internal_key: TrackKey,
}

#[derive(Debug, Default)]
struct CompositeManifests {
    hls_master: Manifest,
    /// Rendition id -> that video's own HLS media playlist, re-rendered under the composite's
    /// version (see `assemble`: the underlying init and media segments need no such rewrite).
    hls_video: HashMap<String, Manifest>,
    /// External `audio-{n}` (index `n - 1`) -> its playlist, likewise re-rendered.
    hls_audio: Vec<Manifest>,
    hls_iframes: Option<Manifest>,
    hls_subtitle: Manifest,
    dash: Manifest,
}

#[derive(Debug)]
pub(crate) struct CompositeAsset {
    video: Vec<VideoEntry>,
    audio_only: Vec<AudioOnlyEntry>,
    audio: Vec<AudioEntry>,
    subtitles: Vec<Subtitle>,
    version: String,
    rendered: CompositeManifests,
    /// Index into `video`: the rendition I-frame playlists and fragments come from (§1 of TDD
    /// 0006 says scrubbing needs only the lowest-bandwidth rendition).
    iframe_source: usize,
}

impl CompositeAsset {
    fn asset_for(&self, owner: AudioOwner) -> &Arc<PackagedAsset> {
        match owner {
            AudioOwner::Video(index) => &self.video[index].asset,
            AudioOwner::Dedicated(index) => &self.audio_only[index].asset,
        }
    }

    /// Finds the underlying asset and its own internal key for an external `(rendition, key)`.
    fn resolve(
        &self,
        rendition: Option<&str>,
        key: TrackKey,
    ) -> Result<(Arc<PackagedAsset>, TrackKey)> {
        match key.kind {
            TrackKind::Video => {
                let id = rendition.ok_or(Error::NotFound("track does not exist"))?;
                let entry = self
                    .video
                    .iter()
                    .find(|entry| entry.id == id)
                    .ok_or(Error::NotFound("track does not exist"))?;
                Ok((Arc::clone(&entry.asset), TrackKey::VIDEO))
            }
            TrackKind::Audio => {
                if rendition.is_some() {
                    return Err(Error::NotFound("track does not exist"));
                }
                let ordinal = audio_ordinal(key);
                let entry = self
                    .audio
                    .get(
                        ordinal
                            .checked_sub(1)
                            .ok_or(Error::NotFound("track does not exist"))?,
                    )
                    .ok_or(Error::NotFound("track does not exist"))?;
                Ok((Arc::clone(self.asset_for(entry.owner)), entry.internal_key))
            }
        }
    }

    fn media_playlist(&self, rendition: Option<&str>, key: TrackKey) -> Result<Manifest> {
        match key.kind {
            TrackKind::Video => {
                let id = rendition.ok_or(Error::NotFound("track does not exist"))?;
                self.rendered
                    .hls_video
                    .get(id)
                    .cloned()
                    .ok_or(Error::NotFound("track does not exist"))
            }
            TrackKind::Audio => {
                if rendition.is_some() {
                    return Err(Error::NotFound("track does not exist"));
                }
                let ordinal = audio_ordinal(key);
                self.rendered
                    .hls_audio
                    .get(
                        ordinal
                            .checked_sub(1)
                            .ok_or(Error::NotFound("track does not exist"))?,
                    )
                    .cloned()
                    .ok_or(Error::NotFound("track does not exist"))
            }
        }
    }

    fn subtitle(&self, language: &str) -> Result<Bytes> {
        self.subtitles
            .iter()
            .find(|subtitle| subtitle.language.eq_ignore_ascii_case(language))
            .map(|subtitle| subtitle.data.clone())
            .ok_or(Error::NotFound("subtitle does not exist"))
    }
}

/// `TrackKey`'s ordinal is private to `media`; audio keys always spell `audio-{n}`, so this reads
/// it back out through that canonical string instead of needing a crate-visible accessor.
fn audio_ordinal(key: TrackKey) -> usize {
    key.to_string()
        .strip_prefix("audio-")
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Builds a composite from every rendition's already-loaded asset, in the order the mapper
/// listed them. Fails naming the rendition id, per TDD 0006: loading is all or nothing.
/// Splits loaded renditions into video and audio-only, by what their own tracks turned out to
/// hold — the wire answer only gives an id and a location, never a kind (see TDD 0006 §3).
fn classify_renditions(
    renditions: Vec<(String, PackagedAsset)>,
) -> Result<(Vec<VideoEntry>, Vec<AudioOnlyEntry>)> {
    let mut video = Vec::new();
    let mut audio_only = Vec::new();
    for (id, asset) in renditions {
        let has_video = asset
            .index
            .tracks
            .iter()
            .any(|t| t.kind == TrackKind::Video);
        let has_audio = asset
            .index
            .tracks
            .iter()
            .any(|t| t.kind == TrackKind::Audio);
        if has_video {
            video.push(VideoEntry {
                id,
                asset: Arc::new(asset),
            });
        } else if has_audio {
            audio_only.push(AudioOnlyEntry {
                id,
                asset: Arc::new(asset),
            });
        } else {
            return Err(Error::InvalidMedia(format!(
                "rendition `{id}` has neither a video nor an audio track"
            )));
        }
    }
    if video.is_empty() {
        return Err(Error::InvalidMedia(
            "an adaptive asset needs at least one video rendition".to_owned(),
        ));
    }
    Ok((video, audio_only))
}

/// The shared audio group: every audio-only rendition's tracks, in mapper order, or, failing
/// that, the first video rendition that has any (see TDD 0006 §3, Audio).
fn build_audio_group(video: &[VideoEntry], audio_only: &[AudioOnlyEntry]) -> Vec<AudioEntry> {
    if audio_only.is_empty() {
        video
            .iter()
            .position(|entry| {
                entry
                    .asset
                    .index
                    .tracks
                    .iter()
                    .any(|t| t.kind == TrackKind::Audio)
            })
            .map(|index| {
                video[index]
                    .asset
                    .index
                    .tracks
                    .iter()
                    .filter(|t| t.kind == TrackKind::Audio)
                    .map(|t| AudioEntry {
                        owner: AudioOwner::Video(index),
                        internal_key: t.key,
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        audio_only
            .iter()
            .enumerate()
            .flat_map(|(index, entry)| {
                entry
                    .asset
                    .index
                    .tracks
                    .iter()
                    .filter(|t| t.kind == TrackKind::Audio)
                    .map(move |t| AudioEntry {
                        owner: AudioOwner::Dedicated(index),
                        internal_key: t.key,
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

/// The index into `video` of the rendition I-frame playlists and fragments come from: the
/// lowest-bandwidth one, since scrubbing does not need more (TDD 0006 §1).
fn pick_iframe_source(video: &[VideoEntry], version: &str) -> usize {
    video
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let presentation =
                Presentation::new(&entry.asset.index.tracks, &entry.asset.plan, version);
            let bandwidth = presentation.video().map_or(0, |track| {
                presentation.bandwidth(track).map_or(0, |b| b.peak)
            });
            (index, bandwidth)
        })
        .min_by_key(|(_, bandwidth)| *bandwidth)
        .map_or(0, |(index, _)| index)
}

pub(crate) fn assemble(
    renditions: Vec<(String, PackagedAsset)>,
    subtitles: Vec<Subtitle>,
    mapper_version: &str,
) -> Result<CompositeAsset> {
    let (video, audio_only) = classify_renditions(renditions)?;
    check_alignment(&video)?;
    check_key_periods(&video)?;
    let audio = build_audio_group(&video, &audio_only);
    let version = version_of(&video, &audio_only, &subtitles, mapper_version);
    let iframe_source = pick_iframe_source(&video, &version);
    let rendered = render(
        &video,
        &audio_only,
        &audio,
        &subtitles,
        &version,
        iframe_source,
    )?;

    Ok(CompositeAsset {
        video,
        audio_only,
        audio,
        subtitles,
        version,
        rendered,
        iframe_source,
    })
}

/// Every video rendition must cut its segments at the same instants, within one sample's
/// duration of the coarser rendition, so a player can switch between them at a segment boundary.
fn check_alignment(video: &[VideoEntry]) -> Result<()> {
    let Some((first, rest)) = video.split_first() else {
        return Ok(());
    };
    let starts_ms = |entry: &VideoEntry| -> Result<Vec<i128>> {
        let track = entry
            .asset
            .presentation()
            .video()
            .ok_or_else(|| Error::InvalidMedia("rendition has no video track".to_owned()))?;
        entry
            .asset
            .presentation()
            .track_segments(track.id)
            .map(|segment| {
                i128::from(segment.decode_time)
                    .checked_mul(1000)
                    .and_then(|ms| ms.checked_div(i128::from(track.timescale)))
                    .ok_or_else(|| Error::InvalidMedia("segment timing overflow".to_owned()))
            })
            .collect()
    };
    let first_starts = starts_ms(first)?;
    let first_tolerance_ms = sample_tolerance_ms(first)?;
    for entry in rest {
        let starts = starts_ms(entry)?;
        let tolerance_ms = sample_tolerance_ms(entry)?.max(first_tolerance_ms);
        if starts.len() != first_starts.len() {
            return Err(Error::InvalidMedia(format!(
                "renditions `{}` and `{}` have different segment counts ({} vs {})",
                first.id,
                entry.id,
                first_starts.len(),
                starts.len()
            )));
        }
        for (index, (a, b)) in first_starts.iter().zip(&starts).enumerate() {
            if (a - b).abs() > tolerance_ms {
                return Err(Error::InvalidMedia(format!(
                    "renditions `{}` and `{}` diverge at segment {index} ({a} ms vs {b} ms)",
                    first.id, entry.id
                )));
            }
        }
    }
    Ok(())
}

/// Every video rendition must begin each key period at the same segment (TDD 0013, stage 2), so a
/// player switching renditions never finds the same segment under two keys. Renditions agree on
/// boundaries only within a sample (see [`check_alignment`]), so a period asked for between two
/// renditions' boundaries would begin a segment apart.
fn check_key_periods(video: &[VideoEntry]) -> Result<()> {
    let starts = video
        .iter()
        .map(|entry| {
            let starts = entry
                .asset
                .presentation()
                .key_schedule()
                .iter()
                .map(|period| period.first_segment)
                .collect::<Vec<_>>();
            (entry.id.as_str(), starts)
        })
        .collect::<Vec<_>>();
    periods_agree(&starts)
}

/// The first segment of every key period, per rendition, must be the same list for all of them.
fn periods_agree(starts: &[(&str, Vec<u32>)]) -> Result<()> {
    let Some(((first_id, first), rest)) = starts.split_first() else {
        return Ok(());
    };
    for (id, other) in rest {
        if other != first {
            return Err(Error::InvalidMedia(format!(
                "renditions `{first_id}` and `{id}` begin their key periods at different \
                 segments ({first:?} vs {other:?})"
            )));
        }
    }
    Ok(())
}

/// The longest single sample of a rendition's video track, in milliseconds: the tolerance one
/// segment boundary is allowed to drift by against another rendition's.
fn sample_tolerance_ms(entry: &VideoEntry) -> Result<i128> {
    let track = entry
        .asset
        .presentation()
        .video()
        .ok_or_else(|| Error::InvalidMedia("rendition has no video track".to_owned()))?;
    let longest = track
        .samples
        .iter()
        .map(|sample| sample.duration)
        .max()
        .unwrap_or(0);
    i128::from(longest)
        .checked_mul(1000)
        .and_then(|ms| ms.checked_div(i128::from(track.timescale)))
        .ok_or_else(|| Error::InvalidMedia("sample duration overflow".to_owned()))
}

fn version_of(
    video: &[VideoEntry],
    audio_only: &[AudioOnlyEntry],
    subtitles: &[Subtitle],
    mapper_version: &str,
) -> String {
    use std::fmt::Write;

    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let feed = |hasher: &mut Sha256, part: &[u8]| {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    };
    feed(&mut hasher, mapper_version.as_bytes());
    for entry in video {
        feed(&mut hasher, entry.id.as_bytes());
        feed(&mut hasher, entry.asset.version().as_bytes());
    }
    for entry in audio_only {
        feed(&mut hasher, entry.id.as_bytes());
        feed(&mut hasher, entry.asset.version().as_bytes());
    }
    for subtitle in subtitles {
        feed(&mut hasher, subtitle.language.as_bytes());
        feed(&mut hasher, subtitle.label.as_bytes());
        hasher.update([u8::from(subtitle.default), u8::from(subtitle.forced)]);
        feed(&mut hasher, &subtitle.data);
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

fn presentation_for<'a>(asset: &'a PackagedAsset, version: &'a str) -> Presentation<'a> {
    Presentation::new(&asset.index.tracks, &asset.plan, version)
        .with_encryption(asset.encryption())
        .with_aes128(asset.aes128())
}

/// Which underlying asset supplies one member of the shared audio group.
fn asset_for<'a>(
    video: &'a [VideoEntry],
    audio_only: &'a [AudioOnlyEntry],
    owner: AudioOwner,
) -> &'a Arc<PackagedAsset> {
    match owner {
        AudioOwner::Video(index) => &video[index].asset,
        AudioOwner::Dedicated(index) => &audio_only[index].asset,
    }
}

type VideoView<'a> = (&'a VideoEntry, Presentation<'a>, &'a Track, Bandwidth);
type AudioView<'a> = (TrackKey, &'a Track);

/// Every video rendition and every shared-audio member, each with the `Presentation` its
/// playlists and manifest entries are rendered from. Video is sorted ascending by bandwidth.
fn build_views<'a>(
    video: &'a [VideoEntry],
    audio_only: &'a [AudioOnlyEntry],
    audio: &'a [AudioEntry],
    version: &'a str,
) -> Result<(Vec<VideoView<'a>>, Vec<AudioView<'a>>)> {
    let mut video_views = video
        .iter()
        .map(|entry| {
            let presentation = presentation_for(&entry.asset, version);
            let track = presentation.video().ok_or_else(|| {
                Error::InvalidMedia(format!("rendition `{}` has no video track", entry.id))
            })?;
            Ok((entry, presentation, track, presentation.bandwidth(track)?))
        })
        .collect::<Result<Vec<_>>>()?;
    video_views.sort_by_key(|(_, _, _, bandwidth)| bandwidth.peak);

    let audio_views = audio
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let presentation = presentation_for(asset_for(video, audio_only, entry.owner), version);
            // The owner's own key finds the track in its Presentation; the *external*, possibly
            // renumbered key (audio-1, audio-2, ...) is what URLs and playlists actually use.
            let track = presentation.track(entry.internal_key)?;
            let external_key = TrackKey::audio(
                u16::try_from(index + 1)
                    .map_err(|_| Error::InvalidMedia("too many audio renditions".to_owned()))?,
            );
            Ok((external_key, track))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((video_views, audio_views))
}

/// The DASH `Period` body: one adaptation set per video rendition, one per shared-audio member,
/// and one per subtitle.
fn render_dash_body(
    video_views: &[VideoView<'_>],
    audio: &[AudioEntry],
    audio_views: &[AudioView<'_>],
    subtitles: &[Subtitle],
    video: &[VideoEntry],
    audio_only: &[AudioOnlyEntry],
    version: &str,
) -> Result<String> {
    let mut body = String::new();
    for (entry, presentation, track, _) in video_views {
        dash::write_video_adaptation(
            &mut body,
            *presentation,
            track,
            version,
            &format!("video-{}", entry.id),
            dash::Addressing::PLAIN,
        )?;
    }
    for (entry, &(key, track)) in audio.iter().zip(audio_views) {
        dash::write_audio_adaptation(
            &mut body,
            presentation_for(asset_for(video, audio_only, entry.owner), version),
            track,
            version,
            &key.to_string(),
            dash::Addressing::PLAIN,
        )?;
    }
    for subtitle in subtitles {
        dash::write_subtitle_adaptation(&mut body, subtitle, version);
    }
    Ok(body)
}

fn render(
    video: &[VideoEntry],
    audio_only: &[AudioOnlyEntry],
    audio: &[AudioEntry],
    subtitles: &[Subtitle],
    version: &str,
    iframe_source: usize,
) -> Result<CompositeManifests> {
    let (video_views, audio_views) = build_views(video, audio_only, audio, version)?;
    // Every rendition is encrypted with the same keys, so the first one speaks for the asset.
    let encryption = video[0].asset.encryption();

    let adaptive_video = video_views
        .iter()
        .map(|(entry, _, track, bandwidth)| AdaptiveVideo {
            id: &entry.id,
            track,
            bandwidth: *bandwidth,
        })
        .collect::<Vec<_>>();
    let adaptive_audio = audio_views
        .iter()
        .map(|(key, track)| AdaptiveAudio { key: *key, track })
        .collect::<Vec<_>>();

    let iframe_presentation = presentation_for(&video[iframe_source].asset, version);
    let iframe_stream_line = iframe_presentation
        .video()
        .and_then(|track| hls::iframe_stream(iframe_presentation, Some(track)).transpose())
        .transpose()?;
    let hls_iframes = hls::iframe_playlist(iframe_presentation)?.map(Manifest::from);

    let hls_master = Manifest::from(hls::adaptive_master_playlist(
        version,
        &adaptive_video,
        &adaptive_audio,
        subtitles,
        iframe_stream_line.as_deref(),
        encryption,
    )?);

    let hls_video = video
        .iter()
        .map(|entry| {
            let playlist =
                hls::media_playlist(presentation_for(&entry.asset, version), TrackKey::VIDEO)?;
            Ok((entry.id.clone(), Manifest::from(playlist)))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let hls_audio = audio
        .iter()
        .map(|entry| {
            let playlist = hls::media_playlist(
                presentation_for(asset_for(video, audio_only, entry.owner), version),
                entry.internal_key,
            )?;
            Ok(Manifest::from(playlist))
        })
        .collect::<Result<Vec<_>>>()?;

    let hls_subtitle = Manifest::from(hls::subtitle_playlist(iframe_presentation));

    let dash_body = render_dash_body(
        &video_views,
        audio,
        &audio_views,
        subtitles,
        video,
        audio_only,
        version,
    )?;
    let duration = dash::track_duration(video_views[0].2)?;
    let dash = Manifest::from(dash::wrap_manifest(
        &duration,
        &dash_body,
        encryption.is_some(),
    ));

    Ok(CompositeManifests {
        hls_master,
        hls_video,
        hls_audio,
        hls_iframes,
        hls_subtitle,
        dash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renditions_that_start_every_period_together_agree() {
        periods_agree(&[("a", vec![0, 1, 2]), ("b", vec![0, 1, 2])]).unwrap();
        periods_agree(&[("a", Vec::new()), ("b", Vec::new())]).unwrap();
        periods_agree(&[("only", vec![0, 3])]).unwrap();
        periods_agree(&[]).unwrap();
    }

    #[test]
    fn a_period_that_starts_a_segment_apart_is_refused_by_name() {
        let error = periods_agree(&[("a", vec![0, 1, 2]), ("b", vec![0, 2, 3])])
            .unwrap_err()
            .to_string();
        assert!(error.contains("`a`") && error.contains("`b`"), "{error}");
        assert!(
            error.contains("[0, 1, 2]") && error.contains("[0, 2, 3]"),
            "{error}"
        );
    }

    #[test]
    fn a_period_one_rendition_drops_is_refused() {
        // One rendition's last segment starts before the final period; the other's does not.
        assert!(periods_agree(&[("a", vec![0, 1]), ("b", vec![0, 1, 2])]).is_err());
    }
}
