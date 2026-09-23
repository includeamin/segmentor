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
use crate::protocol::{AdaptiveAudio, AdaptiveVideo, Bandwidth, Presentation, dash, hls};
use crate::subtitle::Subtitle;

/// What `state.asset()` serves: one file, or several renditions served as one title.
#[derive(Debug)]
pub(crate) enum ServedAsset {
    Single(Arc<PackagedAsset>),
    Composite(Box<CompositeAsset>),
}

impl ServedAsset {
    pub(crate) fn version(&self) -> &str {
        match self {
            Self::Single(asset) => asset.version(),
            Self::Composite(asset) => &asset.version,
        }
    }

    pub(crate) fn hls_master_playlist(&self) -> Bytes {
        match self {
            Self::Single(asset) => asset.hls_master_playlist(),
            Self::Composite(asset) => asset.rendered.hls_master.clone(),
        }
    }

    pub(crate) fn dash_manifest(&self) -> Bytes {
        match self {
            Self::Single(asset) => asset.dash_manifest(),
            Self::Composite(asset) => asset.rendered.dash.clone(),
        }
    }

    pub(crate) fn hls_iframe_playlist(&self) -> Result<Bytes> {
        match self {
            Self::Single(asset) => asset.hls_iframe_playlist(),
            Self::Composite(asset) => asset
                .rendered
                .hls_iframes
                .clone()
                .ok_or(Error::NotFound("asset has no video track")),
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
        }
    }

    pub(crate) fn hls_subtitle_playlist(&self, language: &str) -> Result<Bytes> {
        match self {
            Self::Single(asset) => asset.hls_subtitle_playlist(language),
            Self::Composite(asset) => {
                asset.subtitle(language)?;
                Ok(asset.rendered.hls_subtitle.clone())
            }
        }
    }

    pub(crate) fn subtitle(&self, language: &str) -> Result<Bytes> {
        match self {
            Self::Single(asset) => asset.subtitle(language),
            Self::Composite(asset) => asset.subtitle(language),
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
        }
    }

    pub(crate) fn hls_media_playlist(
        &self,
        rendition: Option<&str>,
        key: TrackKey,
    ) -> Result<Bytes> {
        match self {
            Self::Single(asset) => {
                if rendition.is_some() {
                    return Err(Error::NotFound("track does not exist"));
                }
                asset.hls_media_playlist(key)
            }
            Self::Composite(asset) => asset.media_playlist(rendition, key),
        }
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
        }
    }

    /// Points a remote rendition at a re-signed URL for the same object. `None` for a plain
    /// asset, or the video rendition id for a composite; the shared audio group is never
    /// rotated this way because it is never itself the `location` a mapper answer names.
    pub(crate) fn update_location(&self, rendition: Option<&str>, url: &reqwest::Url) {
        match (self, rendition) {
            (Self::Single(asset), None) => asset.update_location(url),
            (Self::Composite(asset), Some(id)) => {
                if let Some(entry) = asset.video.iter().find(|entry| entry.id == id) {
                    entry.asset.update_location(url);
                } else if let Some(entry) = asset.audio_only.iter().find(|entry| entry.id == id) {
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
                    + asset.rendered.hls_iframes.as_ref().map_or(0, Bytes::len);
                renditions
                    .saturating_add(rendered as u64)
                    .saturating_add(asset.subtitles.iter().map(|s| s.data.len() as u64).sum())
            }
        }
    }

    /// Track count, for status reporting: every video rendition plus the shared audio group.
    pub(crate) fn track_count(&self) -> usize {
        match self {
            Self::Single(asset) => asset.presentation().tracks().len(),
            Self::Composite(asset) => asset.video.len() + asset.audio.len(),
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
        }
    }

    pub(crate) fn subtitle_count(&self) -> usize {
        match self {
            Self::Single(asset) => asset.subtitle_count(),
            Self::Composite(asset) => asset.subtitles.len(),
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
    hls_master: Bytes,
    /// Rendition id -> that video's own HLS media playlist, re-rendered under the composite's
    /// version (see `assemble`: the underlying init and media segments need no such rewrite).
    hls_video: HashMap<String, Bytes>,
    /// External `audio-{n}` (index `n - 1`) -> its playlist, likewise re-rendered.
    hls_audio: Vec<Bytes>,
    hls_iframes: Option<Bytes>,
    hls_subtitle: Bytes,
    dash: Bytes,
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

    fn media_playlist(&self, rendition: Option<&str>, key: TrackKey) -> Result<Bytes> {
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
        )?;
    }
    for (entry, &(key, track)) in audio.iter().zip(audio_views) {
        dash::write_audio_adaptation(
            &mut body,
            presentation_for(asset_for(video, audio_only, entry.owner), version),
            track,
            version,
            &key.to_string(),
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
    let hls_iframes = hls::iframe_playlist(iframe_presentation)?.map(Bytes::from);

    let hls_master = Bytes::from(hls::adaptive_master_playlist(
        version,
        &adaptive_video,
        &adaptive_audio,
        subtitles,
        iframe_stream_line.as_deref(),
    )?);

    let hls_video = video
        .iter()
        .map(|entry| {
            let playlist =
                hls::media_playlist(presentation_for(&entry.asset, version), TrackKey::VIDEO)?;
            Ok((entry.id.clone(), Bytes::from(playlist)))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let hls_audio = audio
        .iter()
        .map(|entry| {
            let playlist = hls::media_playlist(
                presentation_for(asset_for(video, audio_only, entry.owner), version),
                entry.internal_key,
            )?;
            Ok(Bytes::from(playlist))
        })
        .collect::<Result<Vec<_>>>()?;

    let hls_subtitle = Bytes::from(hls::subtitle_playlist(iframe_presentation));

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
    let dash = Bytes::from(dash::wrap_manifest(&duration, &dash_body));

    Ok(CompositeManifests {
        hls_master,
        hls_video,
        hls_audio,
        hls_iframes,
        hls_subtitle,
        dash,
    })
}
