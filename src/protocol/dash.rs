use std::fmt::Write;

use super::hls::track_language;
use super::{Presentation, SequenceClip};
use crate::error::{Error, Result};
use crate::media::Track;
use crate::subtitle::Subtitle;

/// How a `SegmentTemplate` addresses its init and media segments.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Addressing {
    /// The number of the track's first segment.
    pub(crate) start_number: u32,
    /// The clip whose init segment this is, for a sequence; `None` for a plain `init.mp4`.
    pub(crate) clip: Option<usize>,
    /// The media time at the Period's start, in the track's ticks; `None` leaves it out.
    pub(crate) presentation_time_offset: Option<u64>,
}

impl Addressing {
    /// A single file or an adaptive asset: one Period, numbered from zero.
    pub(crate) const PLAIN: Self = Self {
        start_number: 0,
        clip: None,
        presentation_time_offset: None,
    };
}

fn mpd_open(duration_seconds: &str, encrypted: bool) -> String {
    let namespaces = if encrypted {
        " xmlns:cenc=\"urn:mpeg:cenc:2013\" xmlns:dashif=\"https://dashif.org/CPS\""
    } else {
        ""
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\"{namespaces} type=\"static\" mediaPresentationDuration=\"PT{duration_seconds}S\" minBufferTime=\"PT1.5S\" profiles=\"urn:mpeg:dash:profile:isoff-main:2011\">\n"
    )
}

pub(crate) fn manifest(presentation: Presentation<'_>) -> Result<String> {
    let duration = presentation_duration(presentation)?;
    let version = presentation.version();
    let mut manifest = mpd_open(&duration, presentation.encryption().is_some());
    manifest.push_str("  <Period start=\"PT0S\">\n");
    if let Some(video) = presentation.video() {
        write_video_adaptation(
            &mut manifest,
            presentation,
            video,
            version,
            &video.key.to_string(),
            Addressing::PLAIN,
        )?;
    }
    for audio in presentation.audio_tracks() {
        write_audio_adaptation(
            &mut manifest,
            presentation,
            audio,
            version,
            &audio.key.to_string(),
            Addressing::PLAIN,
        )?;
    }
    for subtitle in presentation.subtitles() {
        write_subtitle_adaptation(&mut manifest, subtitle, version);
    }
    manifest.push_str("  </Period>\n</MPD>\n");
    Ok(manifest)
}

/// `id` is the `Representation`'s id and, via `$RepresentationID$` in its `SegmentTemplate`, the
/// path segment its init and media URLs are served under. For a plain asset that is the track's
/// own key (`video`); for one video rendition of an adaptive asset it is `video-{rendition id}`.
pub(crate) fn write_video_adaptation(
    manifest: &mut String,
    presentation: Presentation<'_>,
    track: &Track,
    version: &str,
    id: &str,
    addressing: Addressing,
) -> Result<()> {
    let (width, height) = track
        .codec
        .dimensions()
        .ok_or_else(|| Error::InvalidMedia("video track has no dimensions".to_owned()))?;
    let codec = track.codec.codecs();
    writeln!(
        manifest,
        "    <AdaptationSet contentType=\"video\" segmentAlignment=\"true\" startWithSAP=\"1\">"
    )
    .expect("writing to a String cannot fail");
    if let Some(encryption) = presentation.encryption() {
        manifest.push_str(&crate::cenc::dash_content_protection(
            encryption,
            encryption.key_for(track.kind),
        ));
    }
    writeln!(manifest, "      <Representation id=\"{id}\" bandwidth=\"{}\" codecs=\"{codec}\" mimeType=\"video/mp4\" width=\"{width}\" height=\"{height}\">", presentation.bandwidth(track)?.peak)
        .expect("writing to a String cannot fail");
    write_segment_template(manifest, presentation, track, version, addressing);
    manifest.push_str("      </Representation>\n    </AdaptationSet>\n");
    Ok(())
}

/// See [`write_video_adaptation`] on `id`: for the shared audio group it is `audio-{n}`, which
/// may already differ from the source file's own numbering.
pub(crate) fn write_audio_adaptation(
    manifest: &mut String,
    presentation: Presentation<'_>,
    track: &Track,
    version: &str,
    id: &str,
    addressing: Addressing,
) -> Result<()> {
    let (sample_rate, channels) = track
        .codec
        .audio_format()
        .ok_or_else(|| Error::InvalidMedia("audio track has no audio format".to_owned()))?;
    let codec = track.codec.codecs();
    let language =
        track_language(track).map_or_else(String::new, |language| format!(" lang=\"{language}\""));
    writeln!(
        manifest,
        "    <AdaptationSet contentType=\"audio\"{language} segmentAlignment=\"true\">"
    )
    .expect("writing to a String cannot fail");
    if let Some(encryption) = presentation.encryption() {
        manifest.push_str(&crate::cenc::dash_content_protection(
            encryption,
            encryption.key_for(track.kind),
        ));
    }
    writeln!(manifest, "      <Representation id=\"{id}\" bandwidth=\"{}\" codecs=\"{codec}\" mimeType=\"audio/mp4\" audioSamplingRate=\"{sample_rate}\">", presentation.bandwidth(track)?.peak)
        .expect("writing to a String cannot fail");
    writeln!(manifest, "        <AudioChannelConfiguration schemeIdUri=\"urn:mpeg:dash:23003:3:audio_channel_configuration:2011\" value=\"{channels}\" />")
        .expect("writing to a String cannot fail");
    write_segment_template(manifest, presentation, track, version, addressing);
    manifest.push_str("      </Representation>\n    </AdaptationSet>\n");
    Ok(())
}

/// A static MPD header and footer around adaptation sets the caller has already written, for an
/// adaptive asset whose renditions come from several `Presentation`s.
pub(crate) fn wrap_manifest(duration_seconds: &str, body: &str, encrypted: bool) -> String {
    format!(
        "{}  <Period start=\"PT0S\">\n{body}  </Period>\n</MPD>\n",
        mpd_open(duration_seconds, encrypted)
    )
}

/// See [`presentation_duration`], generalized to a single track when there is no whole
/// `Presentation` to ask (an adaptive asset's renditions each have their own).
pub(crate) fn track_duration(track: &Track) -> Result<String> {
    let milliseconds = track
        .duration
        .checked_mul(1000)
        .and_then(|duration| duration.checked_div(u64::from(track.timescale)))
        .ok_or_else(|| Error::InvalidMedia("track duration overflow".to_owned()))?;
    Ok(format!(
        "{}.{:03}",
        milliseconds / 1000,
        milliseconds % 1000
    ))
}

/// A sidecar `WebVTT` file as a text adaptation set that names the file directly.
pub(crate) fn write_subtitle_adaptation(manifest: &mut String, subtitle: &Subtitle, version: &str) {
    let mut roles = String::new();
    if subtitle.forced {
        roles.push_str(
            "        <Role schemeIdUri=\"urn:mpeg:dash:role:2011\" value=\"forced-subtitle\" />\n",
        );
    } else {
        roles.push_str(
            "        <Role schemeIdUri=\"urn:mpeg:dash:role:2011\" value=\"subtitle\" />\n",
        );
    }
    if subtitle.default {
        roles.push_str("        <Role schemeIdUri=\"urn:mpeg:dash:role:2011\" value=\"main\" />\n");
    }
    writeln!(
        manifest,
        "    <AdaptationSet contentType=\"text\" lang=\"{language}\" mimeType=\"text/vtt\">\n{roles}      <Representation id=\"subtitles-{language}\" bandwidth=\"256\">\n        <BaseURL>subtitles/{language}/sub.vtt?v={version}</BaseURL>\n      </Representation>\n    </AdaptationSet>",
        language = subtitle.language
    )
    .expect("writing to a String cannot fail");
}

fn write_segment_template(
    manifest: &mut String,
    presentation: Presentation<'_>,
    track: &Track,
    version: &str,
    addressing: Addressing,
) {
    let init = addressing.clip.map_or_else(
        || "$RepresentationID$/init.mp4".to_owned(),
        |clip| format!("$RepresentationID$/clips/{clip}/init.mp4"),
    );
    let offset = addressing
        .presentation_time_offset
        .map_or_else(String::new, |offset| {
            format!(" presentationTimeOffset=\"{offset}\"")
        });
    writeln!(manifest, "        <SegmentTemplate timescale=\"{}\"{offset} startNumber=\"{}\" initialization=\"{init}?v={version}\" media=\"$RepresentationID$/segments/$Number$/media.m4s?v={version}\">", track.timescale, addressing.start_number)
        .expect("writing to a String cannot fail");
    manifest.push_str("          <SegmentTimeline>\n");
    for (index, segment) in presentation.track_segments(track.id).enumerate() {
        // Only the first entry states its start; the rest follow on from it. Files with an edit
        // list start after zero, so leaving `t` out would misplace the whole track.
        let start = if index == 0 {
            format!(" t=\"{}\"", segment.decode_time)
        } else {
            String::new()
        };
        writeln!(
            manifest,
            "            <S{start} d=\"{}\" />",
            segment.duration
        )
        .expect("writing to a String cannot fail");
    }
    manifest.push_str("          </SegmentTimeline>\n        </SegmentTemplate>\n");
}

/// A static MPD with one `Period` per clip of a sequence (TDD 0008, DASH). Each Period starts at
/// its clip's position on the sequence timeline, and each `presentationTimeOffset` is that same
/// position in the track's own ticks, because the clips' timestamps continue from one to the next.
pub(crate) fn sequence_manifest(
    clips: &[SequenceClip<'_>],
    total_nanos: u64,
    version: &str,
) -> Result<String> {
    let encrypted = clips
        .first()
        .and_then(|clip| clip.presentation.encryption())
        .is_some();
    let mut manifest = mpd_open(&nanos_as_seconds(total_nanos), encrypted);
    for (position, clip) in clips.iter().enumerate() {
        writeln!(
            manifest,
            "  <Period id=\"clip-{position}\" start=\"PT{}S\">",
            nanos_as_seconds(clip.start_nanos)
        )
        .expect("writing to a String cannot fail");
        let presentation = clip.presentation;
        if let Some(video) = presentation.video() {
            write_video_adaptation(
                &mut manifest,
                presentation,
                video,
                version,
                &video.key.to_string(),
                clip_addressing(clip, position, video)?,
            )?;
        }
        for audio in presentation.audio_tracks() {
            write_audio_adaptation(
                &mut manifest,
                presentation,
                audio,
                version,
                &audio.key.to_string(),
                clip_addressing(clip, position, audio)?,
            )?;
        }
        manifest.push_str("  </Period>\n");
    }
    manifest.push_str("</MPD>\n");
    Ok(manifest)
}

/// The clip's start in `track`'s ticks, rounded down exactly as `clip::trim` places the clip, so
/// the offset and the first fragment's decode time agree.
fn clip_addressing(clip: &SequenceClip<'_>, position: usize, track: &Track) -> Result<Addressing> {
    let offset =
        u64::try_from(u128::from(clip.start_nanos) * u128::from(track.timescale) / 1_000_000_000)
            .map_err(|_| Error::InvalidMedia("period offset overflow".to_owned()))?;
    Ok(Addressing {
        start_number: clip.first_segment,
        clip: Some(position),
        presentation_time_offset: Some(offset),
    })
}

fn nanos_as_seconds(nanos: u64) -> String {
    let milliseconds = nanos / 1_000_000;
    format!("{}.{:03}", milliseconds / 1000, milliseconds % 1000)
}

fn presentation_duration(presentation: Presentation<'_>) -> Result<String> {
    let milliseconds = presentation
        .tracks()
        .iter()
        .map(|track| {
            track
                .duration
                .checked_mul(1000)
                .and_then(|duration| duration.checked_div(u64::from(track.timescale)))
                .ok_or_else(|| Error::InvalidMedia("presentation duration overflow".to_owned()))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .ok_or_else(|| Error::InvalidMedia("asset contains no tracks".to_owned()))?;
    Ok(format!(
        "{}.{:03}",
        milliseconds / 1000,
        milliseconds % 1000
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fixtures::Loaded;

    #[test]
    fn an_encrypted_manifest_declares_its_namespaces_and_protection() {
        let loaded = Loaded::h264_aac();
        let encryption = crate::cenc::tests_support::sample_encryption();

        let manifest = manifest(loaded.presentation().with_encryption(Some(&encryption))).unwrap();

        assert!(
            manifest.contains(
                r#"xmlns:cenc="urn:mpeg:cenc:2013" xmlns:dashif="https://dashif.org/CPS""#
            ),
            "{manifest}"
        );
        assert_eq!(
            manifest.matches(r#"value="cbcs""#).count(),
            2,
            "one per adaptation set: {manifest}"
        );
        assert!(
            !self::manifest(loaded.presentation())
                .unwrap()
                .contains("ContentProtection")
        );
    }

    #[test]
    fn renders_static_manifest_with_both_representations() {
        let loaded = Loaded::h264_aac();

        let manifest = manifest(loaded.presentation()).expect("manifest should render");

        assert!(manifest.contains("type=\"static\""));
        assert!(manifest.contains("id=\"video\""));
        assert!(manifest.contains("id=\"audio-1\""));
        assert!(manifest.contains("$RepresentationID$/segments/$Number$/media.m4s?v="));
        assert_eq!(manifest.matches("<S ").count(), 6);
    }

    #[test]
    fn the_manifest_carries_the_codec_strings_of_the_formats_it_holds() {
        for (name, video, audio) in [
            ("vp9-opus.mp4", "vp09.00.11.08", "opus"),
            ("av1-aac.mp4", "av01.0.00M.08", "mp4a.40.2"),
            ("hevc-aac.mp4", "hvc1.1.6.L60.90", "mp4a.40.2"),
            ("h264-flac.mp4", "avc1.64000d", "fLaC"),
        ] {
            let loaded = Loaded::fixture(name);

            let manifest = manifest(loaded.presentation()).expect("manifest should render");

            assert!(
                manifest.contains(&format!("codecs=\"{video}\"")),
                "{name}: {manifest}"
            );
            assert!(
                manifest.contains(&format!("codecs=\"{audio}\"")),
                "{name}: {manifest}"
            );
            assert!(manifest.contains("audioSamplingRate=\"48000\""), "{name}");
        }
    }

    #[test]
    fn an_audio_only_manifest_has_no_video_adaptation_set() {
        let loaded = Loaded::fixture("aac-only.m4a");

        let manifest = manifest(loaded.presentation()).expect("manifest should render");

        assert!(!manifest.contains("contentType=\"video\""), "{manifest}");
        assert_eq!(manifest.matches("contentType=\"audio\"").count(), 1);
        assert!(
            manifest.contains("mediaPresentationDuration=\"PT3.02"),
            "{manifest}"
        );
    }

    #[test]
    fn renders_one_adaptation_set_per_audio_track() {
        let loaded = Loaded::fixture("h264-aac-two-audio.mp4");

        let manifest = manifest(loaded.presentation()).expect("manifest should render");

        assert_eq!(
            manifest.matches("contentType=\"audio\"").count(),
            2,
            "{manifest}"
        );
        assert!(manifest.contains("lang=\"eng\""));
        assert!(manifest.contains("lang=\"spa\""));
        assert!(manifest.contains("id=\"audio-1\""));
        assert!(manifest.contains("id=\"audio-2\""));
    }

    #[test]
    fn the_first_segment_states_where_a_shifted_timeline_starts() {
        // ffmpeg's default edit lists put audio 2176 ticks after its first kept sample's
        // original position, so its timeline starts at 3200 ticks, not zero.
        let loaded = Loaded::fixture("h264-aac-default-edits.mp4");

        let manifest = manifest(loaded.presentation()).expect("manifest should render");

        let starts = manifest
            .lines()
            .filter(|line| line.contains("<S t="))
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 2, "one first segment per track: {manifest}");
        assert!(
            starts[0].contains("t=\"0\""),
            "video starts at zero: {manifest}"
        );
        assert!(
            starts[1].contains("t=\"3200\""),
            "audio starts at 3200: {manifest}"
        );
        assert_eq!(
            manifest.matches("<S d=").count(),
            4,
            "later segments omit t"
        );
    }

    use crate::protocol::SequenceClip;

    #[test]
    fn a_sequence_manifest_has_one_period_per_clip_placed_on_the_timeline() {
        let (first, second) = (Loaded::h264_aac(), Loaded::h264_aac());
        let clips = [
            SequenceClip {
                presentation: first.presentation(),
                first_segment: 0,
                start_nanos: 0,
            },
            SequenceClip {
                presentation: second.presentation(),
                first_segment: 3,
                start_nanos: 3_066_666_666,
            },
        ];

        let manifest = sequence_manifest(&clips, 6_133_333_332, "v").unwrap();

        assert_eq!(manifest.matches("<Period ").count(), 2, "{manifest}");
        assert!(
            manifest.contains("<Period id=\"clip-0\" start=\"PT0.000S\">"),
            "{manifest}"
        );
        assert!(
            manifest.contains("<Period id=\"clip-1\" start=\"PT3.066S\">"),
            "{manifest}"
        );
        assert!(
            manifest.contains("mediaPresentationDuration=\"PT6.133S\""),
            "{manifest}"
        );
        assert!(
            manifest.contains(
                "startNumber=\"3\" initialization=\"$RepresentationID$/clips/1/init.mp4?v=v\""
            ),
            "{manifest}"
        );
        // 3.066666666 s at 15360 ticks per second, rounded down.
        assert!(
            manifest.contains("timescale=\"15360\" presentationTimeOffset=\"47103\""),
            "{manifest}"
        );
        assert!(
            manifest.contains(
                "presentationTimeOffset=\"0\" startNumber=\"0\" initialization=\"$RepresentationID$/clips/0/init.mp4"
            ),
            "{manifest}"
        );
    }

    #[test]
    fn a_plain_manifest_has_no_presentation_time_offset() {
        let loaded = Loaded::h264_aac();

        let manifest = manifest(loaded.presentation()).unwrap();

        assert!(!manifest.contains("presentationTimeOffset"), "{manifest}");
        assert!(
            manifest.contains("startNumber=\"0\" initialization=\"$RepresentationID$/init.mp4?v=")
        );
    }
}
