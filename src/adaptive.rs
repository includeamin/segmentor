//! Adaptive sequences: every rendition of one asset is a sequence of clips (TDD 0016).
//!
//! The wire is rendition-major and this model is clip-major. Clip *k* of every rendition is a
//! trimmed [`PackagedAsset`]; those go through [`composite::prepare_clip`], which already knows how
//! to check that video renditions can be switched between and how to build the shared audio group.
//! This module then numbers segments across the clips, puts every rendition's clip on one shared
//! timeline, and renders the playlists and manifest that span them.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;

use crate::asset::{Extras, PackagedAsset};
use crate::clip::{self, TimelinePosition, Trailing};
use crate::composite::{self, ClipGrid, ServedAsset, audio_ordinal, sample_tolerance_ms};
use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::fmp4::PreparedSegment;
use crate::media::{AudioOverrides, TrackKey, TrackKind};
use crate::protocol::{Manifest, SequenceAudio, SequenceClip, SequenceVideo, dash, hls};
use crate::sequence::{self, ClipFile, ClipPlan};

/// What one rendition of an adaptive sequence is made from.
pub(crate) struct RenditionPlan {
    pub(crate) id: String,
    /// The language, label and default of the audio it brings, in place of its file's.
    pub(crate) audio: Option<AudioOverrides>,
    /// Its clips, as for a sequence: the file, the window, its own encryption (always `None` in
    /// stage 1), and the tracks it serves.
    pub(crate) clips: Vec<ClipPlan>,
}

#[derive(Debug)]
struct AdaptiveManifests {
    hls_master: Manifest,
    /// Rendition id -> its media playlist across every clip.
    hls_video: HashMap<String, Manifest>,
    /// External `audio-{n}` (index `n - 1`) -> its playlist across every clip.
    hls_audio: Vec<Manifest>,
    dash: Manifest,
}

#[derive(Debug)]
pub(crate) struct AdaptiveSequenceAsset {
    /// One checked grid per clip, in order. Every grid lists the same renditions in the same order.
    grids: Vec<ClipGrid>,
    /// The global number of each clip's first video segment, the same in every video rendition.
    first_video: Vec<u32>,
    /// The same for each member of the shared audio group, which numbers its own segments.
    first_audio: Vec<Vec<u32>>,
    total: TimelinePosition,
    version: String,
    rendered: AdaptiveManifests,
}

/// Trims every rendition's clips onto one shared timeline and builds what is served (TDD 0016).
/// One clip per rendition is an ordinary adaptive asset; two or more are an adaptive sequence.
/// Errors name the rendition and the clip.
pub(crate) fn build(
    asset_id: &str,
    files: &[ClipFile],
    renditions: &[RenditionPlan],
    mapper_version: &str,
    segment_duration_ms: u64,
    limits: &LimitsConfig,
) -> Result<ServedAsset> {
    let clip_count = renditions
        .first()
        .map_or(0, |rendition| rendition.clips.len());
    if clip_count == 0 || renditions.iter().any(|r| r.clips.len() != clip_count) {
        return Err(Error::InvalidMedia(
            "every rendition of an adaptive sequence needs the same, non-zero number of clips"
                .to_owned(),
        ));
    }
    let has_video = video_kinds(files, renditions)?;
    let reference = has_video.iter().position(|&video| video).ok_or_else(|| {
        Error::InvalidMedia("an adaptive asset needs at least one video rendition".to_owned())
    })?;
    let version = version_of(files, renditions, mapper_version);
    let cutter = Cutter {
        asset_id,
        files,
        version: &version,
        segment_duration_ms,
        limits,
    };

    let mut start = TimelinePosition::ZERO;
    let mut starts = Vec::with_capacity(clip_count);
    let mut rows = Vec::with_capacity(clip_count);
    let mut ends = Vec::with_capacity(clip_count);
    for position in 0..clip_count {
        let trailing = if position + 1 == clip_count {
            Trailing::Keep
        } else {
            Trailing::Cut
        };
        let mut next = start;
        let mut row = Vec::with_capacity(renditions.len());
        let mut row_ends = Vec::new();
        for (index, rendition) in renditions.iter().enumerate() {
            let (asset, end) = cutter.cut(rendition, position, start, trailing)?;
            if index == reference {
                next = end;
            }
            if has_video[index] {
                row_ends.push((rendition.id.as_str(), end));
            }
            row.push((rendition.id.clone(), asset));
        }
        starts.push(start);
        rows.push(row);
        ends.push(row_ends);
        start = next;
    }

    // A rendition keeps the same tracks and codecs in every clip.
    for (index, rendition) in renditions.iter().enumerate() {
        let assets = rows.iter().map(|row| &row[index].1).collect::<Vec<_>>();
        sequence::check_layouts(&assets).map_err(|error| {
            Error::InvalidMedia(format!("rendition `{}`: {error}", rendition.id))
        })?;
    }
    if clip_count == 1 {
        let row = rows.pop().expect("checked the length");
        return Ok(ServedAsset::Composite(Box::new(composite::assemble(
            row,
            Vec::new(),
            mapper_version,
        )?)));
    }

    let mut grids = Vec::with_capacity(clip_count);
    for (position, row) in rows.into_iter().enumerate() {
        let grid = composite::prepare_clip(row)
            .map_err(|error| Error::InvalidMedia(format!("clip {position}: {error}")))?;
        let tolerance_ms = grid
            .video
            .iter()
            .map(sample_tolerance_ms)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        check_clip_lengths(position, &ends[position], tolerance_ms)?;
        grids.push(grid);
    }
    Ok(ServedAsset::AdaptiveSequence(Box::new(assemble(
        grids, &starts, start, version,
    )?)))
}

/// What every cut of a rendition's clip needs, so that `build` can ask for one at a time.
struct Cutter<'a> {
    asset_id: &'a str,
    files: &'a [ClipFile],
    version: &'a str,
    segment_duration_ms: u64,
    limits: &'a LimitsConfig,
}

impl Cutter<'_> {
    /// Cuts clip `position` of `rendition` to start at `start`, and assembles it as an asset of
    /// its own. Also returns where the clip ends on the timeline.
    fn cut(
        &self,
        rendition: &RenditionPlan,
        position: usize,
        start: TimelinePosition,
        trailing: Trailing,
    ) -> Result<(PackagedAsset, TimelinePosition)> {
        let named = |error: Error| {
            Error::InvalidMedia(format!(
                "rendition `{}`: clip {position}: {error}",
                rendition.id
            ))
        };
        let (file, window, _, tracks) = &rendition.clips[position];
        let (file, window, tracks) = (*file, *window, *tracks);
        let file = &self.files[file];
        let narrowed = sequence::narrow(&file.parsed.index, tracks).map_err(named)?;
        let cut = clip::trim(&narrowed, window, start, trailing).map_err(named)?;
        tracing::info!(
            event = "clip_trimmed",
            asset.id = self.asset_id,
            rendition = rendition.id,
            clip = position,
            requested.from_ms = window.from_ms,
            requested.to_ms = ?window.to_ms,
            served.from_ms = cut.served_from_ms,
            served.to_ms = cut.served_to_ms,
        );
        let asset = PackagedAsset::assemble(
            file.source.clone(),
            cut.index,
            &file.parsed.metadata,
            self.segment_duration_ms,
            self.limits,
            Extras {
                subtitles: Vec::new(),
                version: Some(self.version.to_owned()),
                encryption: None,
                // An adaptive sequence serves its own playlists; clips are never muxed.
                hls_mux_audio: false,
                hls_aes128: None,
                source_keys: None,
                // Applied above, before the clip was cut.
                tracks: None,
                audio: rendition.audio.clone(),
            },
        )
        .map_err(named)?;
        Ok((asset, cut.end))
    }
}

/// Whether each rendition has video, read from the tracks its first clip serves. It must be the
/// same in every clip: a rendition cannot be video in one clip and audio only in the next.
fn video_kinds(files: &[ClipFile], renditions: &[RenditionPlan]) -> Result<Vec<bool>> {
    let mut kinds = Vec::with_capacity(renditions.len());
    for rendition in renditions {
        let mut first = None;
        for (position, (file, _, _, tracks)) in rendition.clips.iter().enumerate() {
            let narrowed =
                sequence::narrow(&files[*file].parsed.index, *tracks).map_err(|error| {
                    Error::InvalidMedia(format!(
                        "rendition `{}`: clip {position}: {error}",
                        rendition.id
                    ))
                })?;
            let video = narrowed
                .tracks
                .iter()
                .any(|track| track.kind == TrackKind::Video);
            match first {
                None => first = Some(video),
                Some(expected) if expected != video => {
                    let change = if expected {
                        "has video in clip 0 but not"
                    } else {
                        "has no video in clip 0 but has it"
                    };
                    return Err(Error::InvalidMedia(format!(
                        "rendition `{}` {change} in clip {position}",
                        rendition.id
                    )));
                }
                Some(_) => {}
            }
        }
        kinds.push(first.unwrap_or(false));
    }
    Ok(kinds)
}

/// The URL version: the mapper's, and every (rendition, clip) pair's file, window and tracks, so
/// trimming or replacing any one file gives new URLs. A rendition's id and the audio it names
/// are part of it, so renaming or relabelling one changes it too.
fn version_of(files: &[ClipFile], renditions: &[RenditionPlan], mapper_version: &str) -> String {
    use std::fmt::Write;

    let mut seed = mapper_version.to_owned();
    for rendition in renditions {
        write!(seed, "\0{}", rendition.id).expect("writing to a String cannot fail");
        if let Some(audio) = &rendition.audio {
            write!(
                seed,
                ":{:?}:{:?}:{}",
                audio.language, audio.label, audio.default
            )
            .expect("writing to a String cannot fail");
        }
    }
    clip::version_of(
        &seed,
        None,
        renditions.iter().flat_map(|rendition| {
            rendition
                .clips
                .iter()
                .map(|(file, window, encryption, tracks)| {
                    (
                        &files[*file].parsed.index.source,
                        *window,
                        encryption.as_deref(),
                        *tracks,
                    )
                })
        }),
    )
}

/// Every video rendition's clip must end where the first one's does, within `tolerance_ms`, so the
/// next clip starts at the same time in each. Segment starts agreeing is not enough: ends that
/// differ inside the last segment would drift every later boundary.
fn check_clip_lengths(
    position: usize,
    ends: &[(&str, TimelinePosition)],
    tolerance_ms: i128,
) -> Result<()> {
    let Some(&(first_id, first_end)) = ends.first() else {
        return Ok(());
    };
    let first_ms = i128::from(first_end.millis());
    for &(id, end) in &ends[1..] {
        let end_ms = i128::from(end.millis());
        if (end_ms - first_ms).abs() > tolerance_ms {
            return Err(Error::InvalidMedia(format!(
                "renditions `{first_id}` and `{id}` differ in the length of clip {position} \
                 ({first_ms} ms vs {end_ms} ms)"
            )));
        }
    }
    Ok(())
}

/// Each clip's first segment number when the clips' segments are counted one after another.
fn cumulative(counts: impl IntoIterator<Item = Result<usize>>) -> Result<Vec<u32>> {
    let mut next = 0u32;
    let mut firsts = Vec::new();
    for count in counts {
        firsts.push(next);
        next = u32::try_from(count?)
            .ok()
            .and_then(|count| next.checked_add(count))
            .ok_or_else(|| Error::InvalidMedia("a sequence has too many segments".to_owned()))?;
    }
    Ok(firsts)
}

/// The asset that holds one member of the shared audio group in one clip.
fn owner_asset(grid: &ClipGrid, owner: composite::AudioOwner) -> &Arc<PackagedAsset> {
    match owner {
        composite::AudioOwner::Video(index) => &grid.video[index].asset,
        composite::AudioOwner::Dedicated(index) => &grid.audio_only[index].asset,
    }
}

fn assemble(
    grids: Vec<ClipGrid>,
    starts: &[TimelinePosition],
    total: TimelinePosition,
    version: String,
) -> Result<AdaptiveSequenceAsset> {
    let first_video = cumulative(
        grids
            .iter()
            .map(|grid| Ok(grid.video[0].asset.plan.segments.len())),
    )?;
    let first_audio = (0..grids[0].audio.len())
        .map(|member| {
            cumulative(grids.iter().map(|grid| {
                grid.audio
                    .get(member)
                    .map(|entry| owner_asset(grid, entry.owner).plan.segments.len())
                    .ok_or_else(|| {
                        Error::InvalidMedia("the audio group differs between clips".to_owned())
                    })
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let rendered = render(&grids, &first_video, &first_audio, starts, total, &version)?;
    Ok(AdaptiveSequenceAsset {
        grids,
        first_video,
        first_audio,
        total,
        version,
        rendered,
    })
}

fn render(
    grids: &[ClipGrid],
    first_video: &[u32],
    first_audio: &[Vec<u32>],
    starts: &[TimelinePosition],
    total: TimelinePosition,
    version: &str,
) -> Result<AdaptiveManifests> {
    let video = grids[0]
        .video
        .iter()
        .enumerate()
        .map(|(rendition, entry)| SequenceVideo {
            id: &entry.id,
            clips: grids
                .iter()
                .enumerate()
                .map(|(position, grid)| SequenceClip {
                    presentation: grid.video[rendition].asset.presentation(),
                    first_segment: first_video[position],
                    start_nanos: starts[position].nanos(),
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    // The first audio is the default unless a mapper flagged another (TDD 0015).
    let default_audio = grids[0]
        .audio
        .iter()
        .position(|entry| owner_asset(&grids[0], entry.owner).audio_default())
        .unwrap_or(0);
    let audio = grids[0]
        .audio
        .iter()
        .enumerate()
        .map(|(member, entry)| {
            let clips = grids
                .iter()
                .enumerate()
                .map(|(position, grid)| {
                    let entry = grid.audio.get(member).ok_or_else(|| {
                        Error::InvalidMedia("the audio group differs between clips".to_owned())
                    })?;
                    Ok(SequenceClip {
                        presentation: owner_asset(grid, entry.owner).presentation(),
                        first_segment: first_audio[member][position],
                        start_nanos: starts[position].nanos(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(SequenceAudio {
                key: TrackKey::audio(
                    u16::try_from(member + 1)
                        .map_err(|_| Error::InvalidMedia("too many audio renditions".to_owned()))?,
                ),
                internal_key: entry.internal_key,
                default: member == default_audio,
                clips,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let hls_video = video
        .iter()
        .map(|entry| {
            let playlist = hls::sequence_media_playlist(&entry.clips, TrackKey::VIDEO, version)?;
            Ok((entry.id.to_owned(), Manifest::from(playlist)))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let hls_audio = audio
        .iter()
        .map(|member| {
            hls::sequence_media_playlist(&member.clips, member.internal_key, version)
                .map(Manifest::from)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(AdaptiveManifests {
        hls_master: Manifest::from(hls::adaptive_sequence_master_playlist(
            version, &video, &audio,
        )?),
        hls_video,
        hls_audio,
        dash: Manifest::from(dash::adaptive_sequence_manifest(
            &video,
            &audio,
            total.nanos(),
            version,
        )?),
    })
}

impl AdaptiveSequenceAsset {
    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    pub(crate) fn hls_master(&self) -> Manifest {
        self.rendered.hls_master.clone()
    }

    pub(crate) fn dash(&self) -> Manifest {
        self.rendered.dash.clone()
    }

    /// `rendition` names the video rendition a `video-{id}` URL asked for; `None` for the shared
    /// `audio-{n}` group, which is never rendition-scoped.
    pub(crate) fn media_playlist(
        &self,
        rendition: Option<&str>,
        key: TrackKey,
    ) -> Result<Manifest> {
        let missing = Error::NotFound("track does not exist");
        match key.kind {
            TrackKind::Video => self
                .rendered
                .hls_video
                .get(rendition.ok_or(missing)?)
                .cloned()
                .ok_or(Error::NotFound("track does not exist")),
            TrackKind::Audio => {
                if rendition.is_some() {
                    return Err(missing);
                }
                audio_ordinal(key)
                    .checked_sub(1)
                    .and_then(|member| self.rendered.hls_audio.get(member))
                    .cloned()
                    .ok_or(Error::NotFound("track does not exist"))
            }
        }
    }

    /// The asset that holds `(rendition, key)` in clip `clip`, and the key the track has inside it.
    fn resolve(
        &self,
        rendition: Option<&str>,
        key: TrackKey,
        clip: usize,
    ) -> Result<(Arc<PackagedAsset>, TrackKey)> {
        let missing = Error::NotFound("track does not exist");
        let grid = self
            .grids
            .get(clip)
            .ok_or(Error::NotFound("clip does not exist"))?;
        match key.kind {
            TrackKind::Video => {
                let id = rendition.ok_or(missing)?;
                let entry = grid
                    .video
                    .iter()
                    .find(|entry| entry.id == id)
                    .ok_or(Error::NotFound("track does not exist"))?;
                Ok((Arc::clone(&entry.asset), TrackKey::VIDEO))
            }
            TrackKind::Audio => {
                if rendition.is_some() {
                    return Err(missing);
                }
                let entry = audio_ordinal(key)
                    .checked_sub(1)
                    .and_then(|member| grid.audio.get(member))
                    .ok_or(Error::NotFound("track does not exist"))?;
                Ok((
                    Arc::clone(owner_asset(grid, entry.owner)),
                    entry.internal_key,
                ))
            }
        }
    }

    pub(crate) fn clip_init_segment(
        &self,
        rendition: Option<&str>,
        key: TrackKey,
        clip: usize,
    ) -> Result<Bytes> {
        let (asset, internal) = self.resolve(rendition, key, clip)?;
        asset.init_segment(internal)
    }

    /// The fragment for global segment `segment_index` of a track, and the asset whose bytes it
    /// reads. A video rendition and each audio member number their own segments.
    pub(crate) fn prepare_segment(
        &self,
        rendition: Option<&str>,
        key: TrackKey,
        segment_index: u32,
    ) -> Result<(Arc<PackagedAsset>, PreparedSegment)> {
        let firsts: &[u32] = match key.kind {
            TrackKind::Video => &self.first_video,
            TrackKind::Audio => audio_ordinal(key)
                .checked_sub(1)
                .and_then(|member| self.first_audio.get(member))
                .ok_or(Error::NotFound("track does not exist"))?,
        };
        // `firsts[0]` is 0, so at least one clip starts at or before any index.
        let clip = firsts.partition_point(|&first| first <= segment_index) - 1;
        let local = segment_index - firsts[clip];
        let (asset, internal) = self.resolve(rendition, key, clip)?;
        let prepared =
            asset.prepare_numbered_segment(internal, local, segment_index.saturating_add(1))?;
        Ok((asset, prepared))
    }

    /// Clips cut from the same file share one source, so rotating any one of them rotates all.
    pub(crate) fn update_location(&self, rendition: &str, clip: usize, url: &reqwest::Url) {
        let Some(grid) = self.grids.get(clip) else {
            return;
        };
        if let Some(entry) = grid.video.iter().find(|entry| entry.id == rendition) {
            entry.asset.update_location(url);
        } else if let Some(entry) = grid.audio_only.iter().find(|entry| entry.id == rendition) {
            entry.asset.update_location(url);
        }
    }

    fn assets(&self) -> impl Iterator<Item = (&str, usize, &Arc<PackagedAsset>)> {
        self.grids.iter().enumerate().flat_map(|(position, grid)| {
            let video = grid
                .video
                .iter()
                .map(move |entry| (entry.id.as_str(), position, &entry.asset));
            let audio = grid
                .audio_only
                .iter()
                .map(move |entry| (entry.id.as_str(), position, &entry.asset));
            video.chain(audio)
        })
    }

    pub(crate) fn log_load_details(&self, asset_id: &str) {
        for (rendition, position, asset) in self.assets() {
            asset.log_load_details(&format!("{asset_id}/{rendition}/clip-{position}"));
        }
    }

    pub(crate) fn index_bytes(&self) -> u64 {
        let assets: u64 = self.assets().map(|(_, _, asset)| asset.index_bytes()).sum();
        let rendered = self.rendered.hls_master.len()
            + self.rendered.dash.len()
            + self
                .rendered
                .hls_video
                .values()
                .chain(&self.rendered.hls_audio)
                .map(Manifest::len)
                .sum::<usize>();
        assets.saturating_add(rendered as u64)
    }

    /// Every video rendition plus the shared audio group.
    pub(crate) fn track_count(&self) -> usize {
        self.grids[0].video.len() + self.grids[0].audio.len()
    }

    /// Video segments across every clip; every video rendition has the same number.
    pub(crate) fn segment_count(&self) -> usize {
        self.grids
            .iter()
            .map(|grid| grid.video[0].asset.plan.segments.len())
            .sum()
    }

    pub(crate) fn clip_count(&self) -> usize {
        self.grids.len()
    }

    /// Rounded through `u32` milliseconds first, like a sequence's, so the conversion never
    /// silently loses precision.
    pub(crate) fn duration_seconds(&self) -> f64 {
        f64::from(u32::try_from(self.total.millis()).unwrap_or(u32::MAX)) / 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::ClipWindow;
    use crate::source::{LocalMediaSource, MediaSourceKind};
    use crate::testutil::fixture;

    const FIRST: ClipWindow = ClipWindow {
        from_ms: 0,
        to_ms: Some(1000),
    };
    const REST: ClipWindow = ClipWindow {
        from_ms: 1000,
        to_ms: None,
    };
    const WHOLE: ClipWindow = ClipWindow {
        from_ms: 0,
        to_ms: None,
    };

    const HIGH: usize = 0;
    const LOW: usize = 1;
    const ENGLISH: usize = 2;
    const MISALIGNED: usize = 3;
    const TWO_TRACKS: usize = 4;

    async fn files() -> Vec<ClipFile> {
        let mut files = Vec::new();
        for name in [
            "rendition-720p.mp4",
            "rendition-480p.mp4",
            "rendition-audio-en.m4a",
            "rendition-misaligned.mp4",
            "aac-two-tracks-only.m4a",
        ] {
            let source =
                MediaSourceKind::Local(Arc::new(LocalMediaSource::open(fixture(name)).unwrap()));
            let parsed = crate::mp4::parse(&source, &LimitsConfig::default())
                .await
                .unwrap();
            files.push(ClipFile { source, parsed });
        }
        files
    }

    fn plan(id: &str, clips: &[(usize, ClipWindow)]) -> RenditionPlan {
        RenditionPlan {
            id: id.to_owned(),
            audio: None,
            clips: clips
                .iter()
                .map(|&(file, window)| (file, window, None, None))
                .collect(),
        }
    }

    /// Two video renditions and one audio-only rendition, each an ad-sized first clip and the rest.
    fn ladder() -> Vec<RenditionPlan> {
        vec![
            plan("720p", &[(HIGH, FIRST), (HIGH, REST)]),
            plan("480p", &[(LOW, FIRST), (LOW, REST)]),
            plan("en", &[(ENGLISH, FIRST), (ENGLISH, REST)]),
        ]
    }

    fn build_from(files: &[ClipFile], renditions: &[RenditionPlan]) -> Result<ServedAsset> {
        build(
            "test",
            files,
            renditions,
            "v1",
            1000,
            &LimitsConfig::default(),
        )
    }

    fn adaptive(asset: &ServedAsset) -> &AdaptiveSequenceAsset {
        match asset {
            ServedAsset::AdaptiveSequence(asset) => asset,
            _ => panic!("expected an adaptive sequence"),
        }
    }

    fn refusal(result: Result<ServedAsset>) -> String {
        match result {
            Err(error) => error.to_string(),
            Ok(_) => panic!("expected the build to be refused"),
        }
    }

    fn text(manifest: &Manifest) -> String {
        String::from_utf8_lossy(manifest).into_owned()
    }

    fn without_version(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(at) = rest.find("?v=") {
            out.push_str(&rest[..at]);
            rest = rest[at + 3..].trim_start_matches(|c: char| c.is_ascii_hexdigit() || c == '.');
        }
        out.push_str(rest);
        out
    }

    #[tokio::test]
    async fn every_rendition_starts_each_clip_at_the_same_position() {
        let files = files().await;
        let asset = build_from(&files, &ladder()).unwrap();

        let starts = adaptive(&asset).grids[1]
            .video
            .iter()
            .map(|entry| {
                let presentation = entry.asset.presentation();
                let track = presentation.video().unwrap();
                let first = presentation.track_segments(track.id).next().unwrap();
                i128::from(first.decode_time) * 1000 / i128::from(track.timescale)
            })
            .collect::<Vec<_>>();

        assert_eq!(starts.len(), 2);
        assert!((starts[0] - starts[1]).abs() <= 1, "{starts:?}");
        // The fixture's video starts a little after zero, so clip 0 holds just under a second.
        assert!(
            (900..=1100).contains(&starts[0]),
            "clip 1 starts where clip 0 ends: {starts:?}"
        );
    }

    #[tokio::test]
    async fn segment_numbers_run_across_clips_and_agree_across_renditions() {
        let files = files().await;
        let asset = build_from(&files, &ladder()).unwrap();
        let adaptive = adaptive(&asset);
        let total = u32::try_from(asset.segment_count()).unwrap();
        assert!(total >= 2, "two clips make at least two segments");
        assert_eq!(
            asset.segment_count(),
            adaptive
                .grids
                .iter()
                .map(|grid| grid.video[0].asset.plan.segments.len())
                .sum::<usize>()
        );

        for id in ["720p", "480p"] {
            let (_, first) = adaptive
                .prepare_segment(Some(id), TrackKey::VIDEO, 0)
                .unwrap();
            let (_, boundary) = adaptive
                .prepare_segment(Some(id), TrackKey::VIDEO, adaptive.first_video[1])
                .unwrap();
            assert_ne!(
                boundary.ranges, first.ranges,
                "{id}: the second clip's bytes"
            );
            let (_, last) = adaptive
                .prepare_segment(Some(id), TrackKey::VIDEO, total - 1)
                .unwrap();
            assert_eq!(
                &last.header[20..24],
                &total.to_be_bytes(),
                "{id}: numbered across the sequence"
            );
            assert!(matches!(
                adaptive.prepare_segment(Some(id), TrackKey::VIDEO, total),
                Err(Error::NotFound(_))
            ));
        }
        assert!(matches!(
            adaptive.prepare_segment(Some("nope"), TrackKey::VIDEO, 0),
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn a_rendition_playlist_lists_every_clip_with_its_own_map() {
        let files = files().await;
        let asset = build_from(&files, &ladder()).unwrap();
        let adaptive = adaptive(&asset);

        let video = text(
            &adaptive
                .media_playlist(Some("720p"), TrackKey::VIDEO)
                .unwrap(),
        );
        let audio = text(&adaptive.media_playlist(None, TrackKey::audio(1)).unwrap());

        for playlist in [&video, &audio] {
            assert_eq!(
                playlist.matches("#EXT-X-DISCONTINUITY").count(),
                1,
                "{playlist}"
            );
            assert_eq!(playlist.matches("#EXT-X-MAP").count(), 2, "{playlist}");
            assert!(playlist.contains("clips/1/init.mp4"), "{playlist}");
        }
        assert!(
            adaptive
                .clip_init_segment(Some("720p"), TrackKey::VIDEO, 1)
                .is_ok()
        );
        assert!(
            adaptive
                .clip_init_segment(None, TrackKey::audio(1), 1)
                .is_ok()
        );
        assert!(matches!(
            adaptive.clip_init_segment(Some("720p"), TrackKey::VIDEO, 2),
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn a_rendition_with_a_different_clip_length_is_refused_by_name() {
        let files = files().await;
        let longer = ClipWindow {
            from_ms: 0,
            to_ms: Some(2000),
        };
        let renditions = vec![
            plan("720p", &[(HIGH, FIRST), (HIGH, REST)]),
            plan("480p", &[(LOW, longer), (LOW, REST)]),
        ];

        let message = refusal(build_from(&files, &renditions));

        assert!(
            message.contains("720p") && message.contains("480p") && message.contains("clip 0"),
            "{message}"
        );
    }

    #[test]
    fn clip_lengths_that_differ_by_more_than_a_sample_are_refused() {
        let ends = [
            ("720p", TimelinePosition::from_nanos(3_000_000_000)),
            ("480p", TimelinePosition::from_nanos(3_100_000_000)),
        ];

        let message = check_clip_lengths(2, &ends, 34).unwrap_err().to_string();

        assert!(
            message.contains("`720p` and `480p`")
                && message.contains("clip 2")
                && message.contains("3000 ms vs 3100 ms"),
            "{message}"
        );
    }

    #[test]
    fn clip_lengths_within_a_sample_are_accepted() {
        let ends = [
            ("720p", TimelinePosition::from_nanos(3_000_000_000)),
            ("480p", TimelinePosition::from_nanos(3_020_000_000)),
        ];

        check_clip_lengths(0, &ends, 34).unwrap();
    }

    #[tokio::test]
    async fn a_rendition_that_loses_its_video_in_a_clip_is_refused() {
        let files = files().await;
        let renditions = vec![
            plan("720p", &[(HIGH, FIRST), (ENGLISH, REST)]),
            plan("480p", &[(LOW, FIRST), (LOW, REST)]),
        ];

        let message = refusal(build_from(&files, &renditions));

        assert!(
            message.contains("`720p`") && message.contains("video") && message.contains("clip 1"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_rendition_whose_tracks_change_between_clips_is_refused() {
        let files = files().await;
        let renditions = vec![
            plan("720p", &[(HIGH, FIRST), (HIGH, REST)]),
            plan("en", &[(ENGLISH, WHOLE), (TWO_TRACKS, WHOLE)]),
        ];

        let message = refusal(build_from(&files, &renditions));

        assert!(
            message.contains("rendition `en`") && message.contains("different tracks"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn renditions_cut_at_different_keyframes_are_refused() {
        let files = files().await;
        let renditions = vec![
            plan("720p", &[(HIGH, FIRST), (HIGH, REST)]),
            plan("odd", &[(MISALIGNED, FIRST), (MISALIGNED, REST)]),
        ];

        let message = refusal(build_from(&files, &renditions));

        assert!(
            message.contains("720p") && message.contains("odd") && message.contains("clip"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn one_clip_per_rendition_is_served_as_an_ordinary_composite() {
        let files = files().await;
        let renditions = vec![
            plan("720p", &[(HIGH, WHOLE)]),
            plan("480p", &[(LOW, WHOLE)]),
        ];

        let asset = build_from(&files, &renditions).unwrap();

        assert!(matches!(asset, ServedAsset::Composite(_)));
        let plain = composite::assemble(
            vec![
                (
                    "720p".to_owned(),
                    PackagedAsset::load_local(
                        fixture("rendition-720p.mp4"),
                        1000,
                        &LimitsConfig::default(),
                    )
                    .await
                    .unwrap(),
                ),
                (
                    "480p".to_owned(),
                    PackagedAsset::load_local(
                        fixture("rendition-480p.mp4"),
                        1000,
                        &LimitsConfig::default(),
                    )
                    .await
                    .unwrap(),
                ),
            ],
            Vec::new(),
            "v1",
        )
        .unwrap();
        assert_eq!(
            without_version(&text(&asset.hls_master_playlist())),
            without_version(&text(
                &ServedAsset::Composite(Box::new(plain)).hls_master_playlist()
            )),
        );
        let playlist = text(
            &asset
                .hls_media_playlist(Some("720p"), TrackKey::VIDEO)
                .unwrap(),
        );
        assert!(!playlist.contains("#EXT-X-DISCONTINUITY"), "{playlist}");
    }

    #[tokio::test]
    async fn renaming_a_rendition_changes_the_version() {
        let files = files().await;
        let renamed = vec![
            plan("hd", &[(HIGH, FIRST), (HIGH, REST)]),
            plan("sd", &[(LOW, FIRST), (LOW, REST)]),
            plan("en", &[(ENGLISH, FIRST), (ENGLISH, REST)]),
        ];

        let original = build_from(&files, &ladder()).unwrap();
        let renamed = build_from(&files, &renamed).unwrap();

        assert_ne!(original.version(), renamed.version());
    }
}
