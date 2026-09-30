use std::fmt::Write;

use super::{Bandwidth, Presentation, SequenceClip};
use crate::error::{Error, Result};
use crate::media::{Track, TrackKey};
use crate::subtitle::Subtitle;

/// One video rendition of an adaptive asset, already resolved to its own track and bandwidth.
pub(crate) struct AdaptiveVideo<'a> {
    /// The rendition id, as it appears in `video-{id}` URLs.
    pub(crate) id: &'a str,
    pub(crate) track: &'a Track,
    pub(crate) bandwidth: Bandwidth,
}

/// One member of the shared audio group: the external key it is served under (`audio-{n}`,
/// possibly renumbered from the source file's own), and the track that names its codec and
/// language.
pub(crate) struct AdaptiveAudio<'a> {
    pub(crate) key: TrackKey,
    pub(crate) track: &'a Track,
}

/// The master playlist for an adaptive asset: one `#EXT-X-STREAM-INF` per video rendition, all
/// pointing at the one shared audio group, sorted by ascending bandwidth by the caller.
pub(crate) fn adaptive_master_playlist(
    version: &str,
    video: &[AdaptiveVideo<'_>],
    audio: &[AdaptiveAudio<'_>],
    subtitles: &[Subtitle],
    iframe_stream_line: Option<&str>,
) -> Result<String> {
    if video.is_empty() {
        return Err(Error::InvalidMedia(
            "composite asset has no video renditions".to_owned(),
        ));
    }
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    for (index, entry) in audio.iter().enumerate() {
        let language = track_language(entry.track);
        let name = language.map_or_else(
            || format!("Audio {}", index + 1),
            |language| format!("Audio {} ({language})", index + 1),
        );
        let language_attribute =
            language.map_or_else(String::new, |language| format!(",LANGUAGE=\"{language}\""));
        let default = if index == 0 { "YES" } else { "NO" };
        writeln!(
            playlist,
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{name}\"{language_attribute},DEFAULT={default},AUTOSELECT=YES,URI=\"{}/index.m3u8?v={version}\"",
            entry.key
        )
        .expect("writing to a String cannot fail");
    }
    for subtitle in subtitles {
        write_subtitle_rendition(&mut playlist, subtitle, version);
    }
    if let Some(line) = iframe_stream_line {
        playlist.push_str(line);
    }
    let audio_attribute = if audio.is_empty() {
        ""
    } else {
        ",AUDIO=\"audio\""
    };
    let subtitle_attribute = if subtitles.is_empty() {
        ""
    } else {
        ",SUBTITLES=\"subs\""
    };
    for entry in video {
        let codecs = audio.first().map_or_else(
            || entry.track.codec.codecs(),
            |first| {
                format!(
                    "{},{}",
                    entry.track.codec.codecs(),
                    first.track.codec.codecs()
                )
            },
        );
        let resolution = entry
            .track
            .codec
            .dimensions()
            .map_or_else(String::new, |(width, height)| {
                format!(",RESOLUTION={width}x{height}")
            });
        writeln!(
            playlist,
            "#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={},CODECS=\"{codecs}\"{resolution}{audio_attribute}{subtitle_attribute}",
            entry.bandwidth.peak, entry.bandwidth.average
        )
        .expect("writing to a String cannot fail");
        writeln!(playlist, "video-{}/index.m3u8?v={version}", entry.id)
            .expect("writing to a String cannot fail");
    }
    Ok(playlist)
}

pub(crate) fn master_playlist(presentation: Presentation<'_>) -> Result<String> {
    let video = presentation.video();
    let audio_tracks = presentation.audio_tracks().collect::<Vec<_>>();
    // The variant's bandwidth and codecs describe the default (first) audio rendition.
    let audio = audio_tracks.first().copied();
    let version = presentation.version();

    let codecs = video
        .into_iter()
        .chain(audio)
        .map(|track| track.codec.codecs())
        .collect::<Vec<_>>();
    if codecs.is_empty() {
        return Err(Error::InvalidMedia("asset contains no tracks".to_owned()));
    }

    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    // Audio joins the variant as a rendition group whenever there is something to choose
    // between: video plus audio, or several audio tracks. A lone audio track is just the variant.
    let audio_group = audio.is_some() && (video.is_some() || audio_tracks.len() > 1);
    if audio_group {
        for (index, track) in audio_tracks.iter().enumerate() {
            write_audio_rendition(&mut playlist, track, index, version);
        }
    }

    for subtitle in presentation.subtitles() {
        write_subtitle_rendition(&mut playlist, subtitle, version);
    }

    let mut bandwidth = 0u64;
    let mut average_bandwidth = 0u64;
    for track in video.into_iter().chain(audio) {
        let track_bandwidth = presentation.bandwidth(track)?;
        bandwidth = bandwidth
            .checked_add(track_bandwidth.peak)
            .ok_or_else(|| Error::InvalidMedia("bandwidth overflow".to_owned()))?;
        average_bandwidth = average_bandwidth
            .checked_add(track_bandwidth.average)
            .ok_or_else(|| Error::InvalidMedia("bandwidth overflow".to_owned()))?;
    }
    let resolution = video
        .and_then(|track| track.codec.dimensions())
        .map_or_else(String::new, |(width, height)| {
            format!(",RESOLUTION={width}x{height}")
        });
    if let Some(iframes) = iframe_stream(presentation, video)? {
        playlist.push_str(&iframes);
    }
    let audio_attribute = if audio_group { ",AUDIO=\"audio\"" } else { "" };
    let subtitle_attribute = if presentation.subtitles().is_empty() {
        ""
    } else {
        ",SUBTITLES=\"subs\""
    };
    writeln!(
        playlist,
        "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},AVERAGE-BANDWIDTH={average_bandwidth},CODECS=\"{}\"{resolution}{audio_attribute}{subtitle_attribute}",
        codecs.join(",")
    )
    .expect("writing to a String cannot fail");
    let variant = video.or(audio).map_or(TrackKey::VIDEO, |track| track.key);
    writeln!(playlist, "{variant}/index.m3u8?v={version}")
        .expect("writing to a String cannot fail");
    Ok(playlist)
}

pub(crate) fn media_playlist(presentation: Presentation<'_>, key: TrackKey) -> Result<String> {
    let track = presentation.track(key)?;
    let version = presentation.version();
    let target_duration = target_duration(presentation, track);
    let mut playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target_duration}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init.mp4?v={version}\"\n"
    );
    write_segment_entries(&mut playlist, presentation, track, 0, version)?;
    playlist.push_str("#EXT-X-ENDLIST\n");
    Ok(playlist)
}

/// The longest segment of `track`, in whole seconds rounded up, as `#EXT-X-TARGETDURATION` needs.
fn target_duration(presentation: Presentation<'_>, track: &Track) -> u64 {
    presentation
        .track_segments(track.id)
        .map(|segment| segment.duration.div_ceil(u64::from(track.timescale)))
        .max()
        .unwrap_or(1)
}

/// One `#EXTINF` and URI per segment of `track`, numbered from `first_number`.
fn write_segment_entries(
    playlist: &mut String,
    presentation: Presentation<'_>,
    track: &Track,
    first_number: u32,
    version: &str,
) -> Result<()> {
    for (offset, segment) in presentation.track_segments(track.id).enumerate() {
        let milliseconds = segment
            .duration
            .checked_mul(1000)
            .and_then(|duration| duration.checked_div(u64::from(track.timescale)))
            .ok_or_else(|| Error::InvalidMedia("segment duration overflow".to_owned()))?;
        let number = u64::try_from(offset)
            .ok()
            .and_then(|offset| offset.checked_add(u64::from(first_number)))
            .ok_or_else(|| Error::InvalidMedia("segment number overflow".to_owned()))?;
        writeln!(
            playlist,
            "#EXTINF:{}.{:03},\nsegments/{number}/media.m4s?v={version}",
            milliseconds / 1000,
            milliseconds % 1000
        )
        .expect("writing to a String cannot fail");
    }
    Ok(())
}

/// One track's playlist across every clip of a sequence: each clip names its own init segment,
/// and a discontinuity separates it from the one before, since the encoding may change there.
pub(crate) fn sequence_media_playlist(
    clips: &[SequenceClip<'_>],
    key: TrackKey,
    version: &str,
) -> Result<String> {
    let mut target = 1;
    for clip in clips {
        target = target.max(target_duration(
            clip.presentation,
            clip.presentation.track(key)?,
        ));
    }
    let mut playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n"
    );
    for (position, clip) in clips.iter().enumerate() {
        let track = clip.presentation.track(key)?;
        if position > 0 {
            playlist.push_str("#EXT-X-DISCONTINUITY\n");
        }
        writeln!(
            playlist,
            "#EXT-X-MAP:URI=\"clips/{position}/init.mp4?v={version}\""
        )
        .expect("writing to a String cannot fail");
        write_segment_entries(
            &mut playlist,
            clip.presentation,
            track,
            clip.first_segment,
            version,
        )?;
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    Ok(playlist)
}

/// What the one `#EXT-X-STREAM-INF` of a sequence declares, covering every clip.
struct SequenceVariant {
    peak: u64,
    average: u64,
    codecs: String,
    resolution: Option<(u16, u16)>,
}

/// The master playlist of a sequence: one variant whose attributes cover every clip, since a
/// player reads them once for the whole stream (TDD 0008, HLS). No I-frame playlist is offered.
pub(crate) fn sequence_master_playlist(
    clips: &[SequenceClip<'_>],
    version: &str,
) -> Result<String> {
    let first = clips
        .first()
        .ok_or_else(|| Error::InvalidMedia("a sequence has no clips".to_owned()))?
        .presentation;
    let video_key = first.video().map(|track| track.key);
    let audio_keys = first
        .audio_tracks()
        .map(|track| track.key)
        .collect::<Vec<_>>();
    let audio_group = !audio_keys.is_empty() && (video_key.is_some() || audio_keys.len() > 1);

    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    if audio_group {
        for (index, key) in audio_keys.iter().enumerate() {
            // The first clip whose track names a language labels the rendition, so an unlabelled
            // pre-roll does not hide the main content's language.
            let track = match clips
                .iter()
                .filter_map(|clip| clip.presentation.track(*key).ok())
                .find(|track| track_language(track).is_some())
            {
                Some(track) => track,
                None => first.track(*key)?,
            };
            write_audio_rendition(&mut playlist, track, index, version);
        }
    }
    let variant = sequence_variant(clips)?;
    let resolution = variant
        .resolution
        .map_or_else(String::new, |(width, height)| {
            format!(",RESOLUTION={width}x{height}")
        });
    let audio_attribute = if audio_group { ",AUDIO=\"audio\"" } else { "" };
    writeln!(
        playlist,
        "#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={},CODECS=\"{}\"{resolution}{audio_attribute}",
        variant.peak, variant.average, variant.codecs
    )
    .expect("writing to a String cannot fail");
    let entry = video_key
        .or_else(|| audio_keys.first().copied())
        .unwrap_or(TrackKey::VIDEO);
    writeln!(playlist, "{entry}/index.m3u8?v={version}").expect("writing to a String cannot fail");
    Ok(playlist)
}

/// The highest peak of any clip; the average weighted by each clip's duration; every distinct
/// codec string ("every media format present in any Media Segment", RFC 8216); and the largest
/// clip's resolution. Like the single-file master, it counts video plus the default audio.
fn sequence_variant(clips: &[SequenceClip<'_>]) -> Result<SequenceVariant> {
    let overflow = || Error::InvalidMedia("bandwidth overflow".to_owned());
    let (mut peak, mut weighted, mut total_ms) = (0u64, 0u128, 0u128);
    let (mut video_codecs, mut audio_codecs) = (Vec::<String>::new(), Vec::<String>::new());
    let mut resolution: Option<(u16, u16)> = None;
    for clip in clips {
        let presentation = clip.presentation;
        let video = presentation.video();
        let audio = presentation.audio_tracks().next();
        let (mut clip_peak, mut clip_average) = (0u64, 0u64);
        for track in video.into_iter().chain(audio) {
            let bandwidth = presentation.bandwidth(track)?;
            clip_peak = clip_peak.checked_add(bandwidth.peak).ok_or_else(overflow)?;
            clip_average = clip_average
                .checked_add(bandwidth.average)
                .ok_or_else(overflow)?;
        }
        peak = peak.max(clip_peak);
        let reference = video
            .or(audio)
            .ok_or_else(|| Error::InvalidMedia("a clip has no tracks".to_owned()))?;
        let milliseconds = u128::from(reference.duration) * 1000 / u128::from(reference.timescale);
        weighted = weighted
            .checked_add(u128::from(clip_average) * milliseconds)
            .ok_or_else(overflow)?;
        total_ms = total_ms.checked_add(milliseconds).ok_or_else(overflow)?;
        if let Some(track) = video {
            push_distinct(&mut video_codecs, track.codec.codecs());
            if let Some((width, height)) = track.codec.dimensions()
                && resolution.is_none_or(|(best_width, best_height)| {
                    u32::from(width) * u32::from(height)
                        > u32::from(best_width) * u32::from(best_height)
                })
            {
                resolution = Some((width, height));
            }
        }
        if let Some(track) = audio {
            push_distinct(&mut audio_codecs, track.codec.codecs());
        }
    }
    let average =
        u64::try_from(weighted.checked_div(total_ms).unwrap_or(0)).map_err(|_| overflow())?;
    let codecs = video_codecs
        .into_iter()
        .chain(audio_codecs)
        .collect::<Vec<_>>()
        .join(",");
    Ok(SequenceVariant {
        peak,
        average,
        codecs,
        resolution,
    })
}

fn push_distinct(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// The playlist that lists a subtitle file: the whole file as a single segment as long as the
/// presentation, which is valid for VOD. It is the same for every language, because it names the
/// file relative to its own location.
pub(crate) fn subtitle_playlist(presentation: Presentation<'_>) -> String {
    let seconds = presentation.duration_seconds().max(1);
    let version = presentation.version();
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{seconds}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:{seconds}.000,\nsub.vtt?v={version}\n#EXT-X-ENDLIST\n"
    )
}

/// The I-frame playlist of the video track: one entry per keyframe, each its own one-sample
/// fragment. `None` for an asset without video.
pub(crate) fn iframe_playlist(presentation: Presentation<'_>) -> Result<Option<String>> {
    let Some(track) = presentation.video() else {
        return Ok(None);
    };
    let version = presentation.version();
    let frames = keyframes(track)?;
    let target_duration = frames
        .entries
        .iter()
        .map(|frame| frame.interval.div_ceil(u64::from(track.timescale)))
        .max()
        .unwrap_or(1);
    let mut playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target_duration}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-I-FRAMES-ONLY\n#EXT-X-MAP:URI=\"init.mp4?v={version}\"\n"
    );
    for (index, frame) in frames.entries.iter().enumerate() {
        let milliseconds = frame
            .interval
            .checked_mul(1000)
            .and_then(|interval| interval.checked_div(u64::from(track.timescale)))
            .ok_or_else(|| Error::InvalidMedia("keyframe interval overflow".to_owned()))?;
        writeln!(
            playlist,
            "#EXTINF:{}.{:03},\niframes/{index}/media.m4s?v={version}",
            milliseconds / 1000,
            milliseconds % 1000
        )
        .expect("writing to a String cannot fail");
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    Ok(Some(playlist))
}

/// The `#EXT-X-I-FRAME-STREAM-INF` line for the video track, or `None` when there is no video.
pub(crate) fn iframe_stream(
    presentation: Presentation<'_>,
    video: Option<&Track>,
) -> Result<Option<String>> {
    let Some(track) = video else {
        return Ok(None);
    };
    let frames = keyframes(track)?;
    let (Some(peak), Some(average)) = (
        frames.peak_bandwidth(track.timescale),
        frames.average_bandwidth(track.timescale),
    ) else {
        return Ok(None);
    };
    let resolution = track
        .codec
        .dimensions()
        .map_or_else(String::new, |(width, height)| {
            format!(",RESOLUTION={width}x{height}")
        });
    Ok(Some(format!(
        "#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH={peak},AVERAGE-BANDWIDTH={average},CODECS=\"{}\"{resolution},URI=\"video/iframes.m3u8?v={}\"\n",
        track.codec.codecs(),
        presentation.version()
    )))
}

/// The sync samples of a track: where each is, how long it stays on screen (until the next one),
/// and how big it is.
struct Keyframes {
    entries: Vec<Keyframe>,
}

struct Keyframe {
    /// Ticks from this keyframe to the next, or to the end of the track.
    interval: u64,
    bytes: u64,
}

fn keyframes(track: &Track) -> Result<Keyframes> {
    let overflow = || Error::InvalidMedia("keyframe interval overflow".to_owned());
    let sync = track
        .samples
        .iter()
        .filter(|sample| sample.is_sync)
        .collect::<Vec<_>>();
    let end = track
        .samples
        .last()
        .map(|last| last.decode_time.checked_add(u64::from(last.duration)))
        .ok_or_else(overflow)?
        .ok_or_else(overflow)?;
    let mut entries = Vec::with_capacity(sync.len());
    for (position, sample) in sync.iter().enumerate() {
        let next = sync.get(position + 1).map_or(end, |next| next.decode_time);
        entries.push(Keyframe {
            interval: next.checked_sub(sample.decode_time).ok_or_else(overflow)?,
            bytes: u64::from(sample.size),
        });
    }
    Ok(Keyframes { entries })
}

impl Keyframes {
    /// The largest keyframe, in bits per second over the interval it stands for.
    fn peak_bandwidth(&self, timescale: u32) -> Option<u64> {
        self.entries
            .iter()
            .filter_map(|frame| bits_per_second(frame.bytes, frame.interval, timescale))
            .max()
    }

    /// All keyframes' bytes over the time they cover.
    fn average_bandwidth(&self, timescale: u32) -> Option<u64> {
        let bytes = self.entries.iter().map(|frame| frame.bytes).sum();
        let ticks = self.entries.iter().map(|frame| frame.interval).sum();
        bits_per_second(bytes, ticks, timescale)
    }
}

fn bits_per_second(bytes: u64, ticks: u64, timescale: u32) -> Option<u64> {
    bytes
        .checked_mul(8)?
        .checked_mul(u64::from(timescale))?
        .checked_div(ticks)
}

/// One `#EXT-X-MEDIA:TYPE=SUBTITLES` line. `AUTOSELECT` follows `DEFAULT`, and `FORCED` marks
/// a track a player should show even when the viewer has not asked for subtitles.
fn write_subtitle_rendition(playlist: &mut String, subtitle: &Subtitle, version: &str) {
    let flag = |value: bool| if value { "YES" } else { "NO" };
    let name = subtitle.label.replace('"', "'");
    writeln!(
        playlist,
        "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\",LANGUAGE=\"{}\",DEFAULT={},AUTOSELECT={},FORCED={},URI=\"subtitles/{}/index.m3u8?v={version}\"",
        subtitle.language,
        flag(subtitle.default),
        flag(subtitle.default || subtitle.forced),
        flag(subtitle.forced),
        subtitle.language
    )
    .expect("writing to a String cannot fail");
}

/// One `#EXT-X-MEDIA` line. Renditions are named by position, because the handler names encoders
/// write (`SoundHandler`) say nothing to a viewer; the language is added when the file has one.
fn write_audio_rendition(playlist: &mut String, track: &Track, index: usize, version: &str) {
    let number = index + 1;
    let language = track_language(track);
    let name = language.map_or_else(
        || format!("Audio {number}"),
        |language| format!("Audio {number} ({language})"),
    );
    let language_attribute =
        language.map_or_else(String::new, |language| format!(",LANGUAGE=\"{language}\""));
    let default = if index == 0 { "YES" } else { "NO" };
    writeln!(
        playlist,
        "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{name}\"{language_attribute},DEFAULT={default},AUTOSELECT=YES,URI=\"{}/index.m3u8?v={version}\"",
        track.key
    )
    .expect("writing to a String cannot fail");
}

/// The track's three-letter language, or `None` when the file leaves it undetermined or holds
/// something that is not a language code.
pub(crate) fn track_language(track: &Track) -> Option<&str> {
    let language = track.language.as_str();
    (language.len() == 3
        && language.bytes().all(|byte| byte.is_ascii_lowercase())
        && language != "und")
        .then_some(language)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fixtures::Loaded;

    #[test]
    fn master_references_separate_audio_and_video_playlists() {
        let loaded = Loaded::h264_aac();
        let playlist =
            master_playlist(loaded.presentation()).expect("master playlist should render");

        assert!(playlist.contains("CODECS=\"avc1.64000d,mp4a.40.2\""));
        assert!(playlist.contains("AVERAGE-BANDWIDTH="));
        let version = &loaded.version;
        assert!(playlist.contains(&format!("URI=\"audio-1/index.m3u8?v={version}\"")));
        assert!(playlist.contains(&format!("video/index.m3u8?v={version}\n")));
        assert!(
            !playlist.contains('{'),
            "no unexpanded placeholders: {playlist}"
        );
    }

    #[test]
    fn the_iframe_playlist_lists_one_fragment_per_keyframe() {
        let loaded = Loaded::h264_aac();
        let keyframes = loaded.index.tracks[0]
            .samples
            .iter()
            .filter(|sample| sample.is_sync)
            .count();

        let playlist = iframe_playlist(loaded.presentation())
            .expect("playlist should render")
            .expect("a video asset has one");

        assert!(playlist.contains("#EXT-X-I-FRAMES-ONLY\n"), "{playlist}");
        assert!(playlist.contains(&format!("#EXT-X-MAP:URI=\"init.mp4?v={}\"", loaded.version)));
        assert_eq!(playlist.matches("#EXTINF:").count(), keyframes);
        assert!(playlist.contains(&format!("iframes/{}/media.m4s?v=", keyframes - 1)));
        let master = master_playlist(loaded.presentation()).expect("master should render");
        assert!(
            master.contains("#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH="),
            "{master}"
        );
        assert!(master.contains(&format!("URI=\"video/iframes.m3u8?v={}\"", loaded.version)));
    }

    #[test]
    fn an_audio_only_asset_has_no_iframe_playlist() {
        let loaded = Loaded::fixture("aac-only.m4a");

        assert!(
            iframe_playlist(loaded.presentation())
                .expect("rendering should succeed")
                .is_none()
        );
        let master = master_playlist(loaded.presentation()).expect("master should render");
        assert!(!master.contains("I-FRAME"), "{master}");
    }

    #[test]
    fn master_lists_every_audio_track_and_defaults_the_first() {
        let loaded = Loaded::fixture("h264-aac-two-audio.mp4");
        let playlist =
            master_playlist(loaded.presentation()).expect("master playlist should render");
        let version = &loaded.version;

        let renditions = playlist
            .lines()
            .filter(|line| line.starts_with("#EXT-X-MEDIA"))
            .collect::<Vec<_>>();
        assert_eq!(renditions.len(), 2, "{playlist}");
        assert!(
            renditions[0].contains("NAME=\"Audio 1 (eng)\""),
            "{playlist}"
        );
        assert!(renditions[0].contains("LANGUAGE=\"eng\""));
        assert!(renditions[0].contains("DEFAULT=YES"));
        assert!(renditions[0].contains(&format!("URI=\"audio-1/index.m3u8?v={version}\"")));
        assert!(renditions[1].contains("NAME=\"Audio 2 (spa)\""));
        assert!(renditions[1].contains("LANGUAGE=\"spa\""));
        assert!(renditions[1].contains("DEFAULT=NO"));
        assert!(renditions[1].contains(&format!("URI=\"audio-2/index.m3u8?v={version}\"")));
        assert_eq!(playlist.matches("#EXT-X-STREAM-INF").count(), 1);
    }

    #[test]
    fn the_master_lists_the_codecs_of_the_formats_it_carries() {
        for (name, codecs) in [
            ("vp9-opus.mp4", "vp09.00.11.08,opus"),
            ("av1-aac.mp4", "av01.0.00M.08,mp4a.40.2"),
            ("hevc-aac.mp4", "hvc1.1.6.L60.90,mp4a.40.2"),
            ("h264-ac3.mp4", "avc1.64000d,ac-3"),
            ("h264-eac3.mp4", "avc1.64000d,ec-3"),
            ("h264-flac.mp4", "avc1.64000d,fLaC"),
        ] {
            let loaded = Loaded::fixture(name);

            let playlist =
                master_playlist(loaded.presentation()).expect("master playlist should render");

            assert!(
                playlist.contains(&format!("CODECS=\"{codecs}\"")),
                "{name}: {playlist}"
            );
            assert!(playlist.contains("RESOLUTION=320x180"), "{name}");
        }
    }

    #[test]
    fn an_audio_only_asset_is_one_audio_variant() {
        let loaded = Loaded::fixture("aac-only.m4a");

        let playlist =
            master_playlist(loaded.presentation()).expect("master playlist should render");

        assert!(playlist.contains("CODECS=\"mp4a.40.2\""), "{playlist}");
        assert!(!playlist.contains("RESOLUTION"), "{playlist}");
        assert!(
            !playlist.contains("EXT-X-MEDIA"),
            "a lone track is the variant: {playlist}"
        );
        assert!(playlist.contains(&format!("\naudio-1/index.m3u8?v={}\n", loaded.version)));
        let media = media_playlist(loaded.presentation(), TrackKey::audio(1)).unwrap();
        assert_eq!(media.matches("#EXTINF:").count(), 3);
    }

    #[test]
    fn several_audio_only_tracks_are_renditions_of_one_variant() {
        let loaded = Loaded::fixture("aac-two-tracks-only.m4a");

        let playlist =
            master_playlist(loaded.presentation()).expect("master playlist should render");

        assert_eq!(playlist.matches("#EXT-X-MEDIA").count(), 2, "{playlist}");
        assert_eq!(playlist.matches("#EXT-X-STREAM-INF").count(), 1);
        assert!(playlist.contains("AUDIO=\"audio\""));
        assert!(!playlist.contains("RESOLUTION"));
        assert!(playlist.contains("LANGUAGE=\"spa\""));
        assert!(playlist.contains(&format!("\naudio-1/index.m3u8?v={}\n", loaded.version)));
    }

    #[test]
    fn a_second_audio_track_has_its_own_media_playlist() {
        let loaded = Loaded::fixture("h264-aac-two-audio.mp4");

        let playlist = media_playlist(loaded.presentation(), TrackKey::audio(2))
            .expect("second audio playlist should render");

        assert!(playlist.contains("#EXT-X-MAP:URI=\"init.mp4?v="));
        assert_eq!(playlist.matches("#EXTINF:").count(), 3);
        assert!(matches!(
            media_playlist(loaded.presentation(), TrackKey::audio(3)),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn an_undetermined_language_is_left_out() {
        let loaded = Loaded::h264_aac();
        let playlist =
            master_playlist(loaded.presentation()).expect("master playlist should render");

        assert!(playlist.contains("NAME=\"Audio 1\""), "{playlist}");
        assert!(!playlist.contains("LANGUAGE"), "{playlist}");
    }

    #[test]
    fn video_playlist_references_init_and_three_segments() {
        let loaded = Loaded::h264_aac();
        let playlist = media_playlist(loaded.presentation(), TrackKey::VIDEO)
            .expect("media playlist should render");

        assert!(playlist.contains("#EXT-X-TARGETDURATION:1"));
        assert!(playlist.contains("#EXT-X-MAP:URI=\"init.mp4?v="));
        assert_eq!(playlist.matches("#EXTINF:1.000,").count(), 3);
        assert!(playlist.contains("segments/2/media.m4s?v="));
        assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn a_missing_track_is_not_found() {
        let loaded = Loaded::h264_aac();
        let presentation = Presentation::new(&loaded.index.tracks[..1], &loaded.plan, "v");

        let error = media_playlist(presentation, TrackKey::audio(1))
            .expect_err("audio is absent from the view");

        assert!(matches!(error, Error::NotFound(_)));
    }

    use crate::protocol::SequenceClip;

    fn clip(loaded: &Loaded, first_segment: u32, start_nanos: u64) -> SequenceClip<'_> {
        SequenceClip {
            presentation: loaded.presentation(),
            first_segment,
            start_nanos,
        }
    }

    #[test]
    fn a_sequence_playlist_maps_each_clip_and_marks_each_boundary() {
        let (first, second) = (Loaded::h264_aac(), Loaded::h264_aac());
        let clips = [clip(&first, 0, 0), clip(&second, 3, 3_000_000_000)];

        let playlist = sequence_media_playlist(&clips, TrackKey::VIDEO, "v").unwrap();

        assert_eq!(playlist.matches("#EXT-X-MAP:").count(), 2, "{playlist}");
        assert_eq!(
            playlist.matches("#EXT-X-DISCONTINUITY").count(),
            1,
            "{playlist}"
        );
        let boundary = playlist.find("#EXT-X-DISCONTINUITY\n").unwrap();
        assert!(playlist.find("segments/2/media.m4s").unwrap() < boundary);
        let second_map = playlist
            .find("#EXT-X-MAP:URI=\"clips/1/init.mp4?v=v\"")
            .unwrap();
        assert!(boundary < second_map);
        assert!(second_map < playlist.find("segments/3/media.m4s").unwrap());
        assert!(playlist.contains("#EXT-X-MAP:URI=\"clips/0/init.mp4?v=v\""));
        assert_eq!(playlist.matches("#EXTINF:").count(), 6);
        assert!(playlist.contains("segments/5/media.m4s?v=v"));
        assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn a_sequence_master_covers_every_clip() {
        let small = Loaded::h264_aac();
        let large = Loaded::fixture("rendition-720p.mp4");
        let hevc = Loaded::fixture("hevc-aac.mp4");
        let clips = [
            clip(&small, 0, 0),
            clip(&large, 3, 3_000_000_000),
            clip(&hevc, 6, 6_000_000_000),
        ];

        let playlist = sequence_master_playlist(&clips, "v").unwrap();

        assert_eq!(
            playlist.matches("#EXT-X-STREAM-INF").count(),
            1,
            "{playlist}"
        );
        assert!(
            playlist.contains("RESOLUTION=640x360"),
            "the largest clip: {playlist}"
        );
        assert!(playlist.contains("hvc1.1.6.L60.90"), "{playlist}");
        assert_eq!(
            playlist.matches("avc1.64000d").count(),
            1,
            "each codec once: {playlist}"
        );
        assert_eq!(playlist.matches("mp4a.40.2").count(), 1, "{playlist}");
        assert!(!playlist.contains("I-FRAME"), "{playlist}");
        let peak = clips
            .iter()
            .map(|clip| {
                let presentation = clip.presentation;
                presentation
                    .video()
                    .into_iter()
                    .chain(presentation.audio_tracks().next())
                    .map(|track| presentation.bandwidth(track).unwrap().peak)
                    .sum::<u64>()
            })
            .max()
            .unwrap();
        assert!(
            playlist.contains(&format!("BANDWIDTH={peak},")),
            "the burstiest clip: {playlist}"
        );
    }

    #[test]
    fn the_first_labelled_clip_names_a_sequences_audio() {
        let (unlabelled, labelled) = (
            Loaded::h264_aac(),
            Loaded::fixture("h264-aac-two-audio.mp4"),
        );
        let clips = [clip(&unlabelled, 0, 0), clip(&labelled, 3, 3_000_000_000)];

        let playlist = sequence_master_playlist(&clips, "v").unwrap();

        assert!(
            playlist.contains("NAME=\"Audio 1 (eng)\",LANGUAGE=\"eng\""),
            "{playlist}"
        );
    }

    #[test]
    fn an_audio_only_sequence_is_one_audio_variant() {
        let (first, second) = (
            Loaded::fixture("aac-only.m4a"),
            Loaded::fixture("aac-only.m4a"),
        );
        let clips = [clip(&first, 0, 0), clip(&second, 3, 3_000_000_000)];

        let master = sequence_master_playlist(&clips, "v").unwrap();
        let media = sequence_media_playlist(&clips, TrackKey::audio(1), "v").unwrap();

        assert!(master.contains("\naudio-1/index.m3u8?v=v\n"), "{master}");
        assert!(!master.contains("RESOLUTION"), "{master}");
        assert!(
            !master.contains("EXT-X-MEDIA"),
            "a lone track is the variant: {master}"
        );
        assert_eq!(media.matches("#EXT-X-DISCONTINUITY").count(), 1, "{media}");
    }
}
