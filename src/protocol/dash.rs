use std::fmt::Write;

use super::Presentation;
use super::hls::track_language;
use crate::error::{Error, Result};
use crate::media::Track;
use crate::subtitle::Subtitle;

pub(crate) fn manifest(presentation: Presentation<'_>) -> Result<String> {
    let duration = presentation_duration(presentation)?;
    let version = presentation.version();
    let mut manifest = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"static\" mediaPresentationDuration=\"PT{duration}S\" minBufferTime=\"PT1.5S\" profiles=\"urn:mpeg:dash:profile:isoff-main:2011\">\n  <Period start=\"PT0S\">\n"
    );
    if let Some(video) = presentation.video() {
        write_video_adaptation(&mut manifest, presentation, video, version)?;
    }
    for audio in presentation.audio_tracks() {
        write_audio_adaptation(&mut manifest, presentation, audio, version)?;
    }
    for subtitle in presentation.subtitles() {
        write_subtitle_adaptation(&mut manifest, subtitle, version);
    }
    manifest.push_str("  </Period>\n</MPD>\n");
    Ok(manifest)
}

fn write_video_adaptation(
    manifest: &mut String,
    presentation: Presentation<'_>,
    track: &Track,
    version: &str,
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
    writeln!(manifest, "      <Representation id=\"{}\" bandwidth=\"{}\" codecs=\"{codec}\" mimeType=\"video/mp4\" width=\"{width}\" height=\"{height}\">", track.key, presentation.bandwidth(track)?.peak)
        .expect("writing to a String cannot fail");
    write_segment_template(manifest, presentation, track, version);
    manifest.push_str("      </Representation>\n    </AdaptationSet>\n");
    Ok(())
}

fn write_audio_adaptation(
    manifest: &mut String,
    presentation: Presentation<'_>,
    track: &Track,
    version: &str,
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
    writeln!(manifest, "      <Representation id=\"{}\" bandwidth=\"{}\" codecs=\"{codec}\" mimeType=\"audio/mp4\" audioSamplingRate=\"{sample_rate}\">", track.key, presentation.bandwidth(track)?.peak)
        .expect("writing to a String cannot fail");
    writeln!(manifest, "        <AudioChannelConfiguration schemeIdUri=\"urn:mpeg:dash:23003:3:audio_channel_configuration:2011\" value=\"{channels}\" />")
        .expect("writing to a String cannot fail");
    write_segment_template(manifest, presentation, track, version);
    manifest.push_str("      </Representation>\n    </AdaptationSet>\n");
    Ok(())
}

/// A sidecar `WebVTT` file as a text adaptation set that names the file directly.
fn write_subtitle_adaptation(manifest: &mut String, subtitle: &Subtitle, version: &str) {
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
) {
    writeln!(manifest, "        <SegmentTemplate timescale=\"{}\" startNumber=\"0\" initialization=\"$RepresentationID$/init.mp4?v={version}\" media=\"$RepresentationID$/segments/$Number$/media.m4s?v={version}\">", track.timescale)
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
}
