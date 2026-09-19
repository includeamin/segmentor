use std::fmt::Write;

use super::Presentation;
use crate::error::{Error, Result};
use crate::media::{CodecConfig, Track, TrackKind};

pub(crate) fn manifest(presentation: Presentation<'_>) -> Result<String> {
    let video = presentation.track(TrackKind::Video)?;
    let audio = presentation.track(TrackKind::Audio).ok();
    let duration = presentation_duration(presentation)?;
    let version = presentation.version();
    let mut manifest = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"static\" mediaPresentationDuration=\"PT{duration}S\" minBufferTime=\"PT1.5S\" profiles=\"urn:mpeg:dash:profile:isoff-main:2011\">\n  <Period start=\"PT0S\">\n"
    );
    write_video_adaptation(&mut manifest, presentation, video, version)?;
    if let Some(audio) = audio {
        write_audio_adaptation(&mut manifest, presentation, audio, version)?;
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
    let CodecConfig::Avc {
        width,
        height,
        profile,
        compatibility,
        level,
        ..
    } = track.codec
    else {
        return Err(Error::Unsupported("DASH video must be H.264"));
    };
    let codec = format!("avc1.{profile:02x}{compatibility:02x}{level:02x}");
    writeln!(
        manifest,
        "    <AdaptationSet contentType=\"video\" segmentAlignment=\"true\" startWithSAP=\"1\">"
    )
    .expect("writing to a String cannot fail");
    writeln!(manifest, "      <Representation id=\"video\" bandwidth=\"{}\" codecs=\"{codec}\" mimeType=\"video/mp4\" width=\"{width}\" height=\"{height}\">", presentation.bandwidth(track)?.peak)
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
    let CodecConfig::Aac {
        sample_rate,
        channels,
    } = track.codec
    else {
        return Err(Error::Unsupported("DASH audio must be AAC"));
    };
    writeln!(
        manifest,
        "    <AdaptationSet contentType=\"audio\" segmentAlignment=\"true\">"
    )
    .expect("writing to a String cannot fail");
    writeln!(manifest, "      <Representation id=\"audio\" bandwidth=\"{}\" codecs=\"mp4a.40.2\" mimeType=\"audio/mp4\" audioSamplingRate=\"{sample_rate}\">", presentation.bandwidth(track)?.peak)
        .expect("writing to a String cannot fail");
    writeln!(manifest, "        <AudioChannelConfiguration schemeIdUri=\"urn:mpeg:dash:23003:3:audio_channel_configuration:2011\" value=\"{channels}\" />")
        .expect("writing to a String cannot fail");
    write_segment_template(manifest, presentation, track, version);
    manifest.push_str("      </Representation>\n    </AdaptationSet>\n");
    Ok(())
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
    for segment in presentation.track_segments(track.id) {
        writeln!(manifest, "            <S d=\"{}\" />", segment.duration)
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
        assert!(manifest.contains("id=\"audio\""));
        assert!(manifest.contains("$RepresentationID$/segments/$Number$/media.m4s?v="));
        assert_eq!(manifest.matches("<S d=").count(), 6);
    }
}
