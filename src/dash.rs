use std::fmt::Write;

use crate::asset::PackagedAsset;
use crate::error::{Error, Result};
use crate::media::{CodecConfig, Track, TrackKind};

pub(crate) fn manifest(asset: &PackagedAsset) -> Result<String> {
    let video = asset.track(TrackKind::Video)?;
    let audio = asset.track(TrackKind::Audio).ok();
    let duration = presentation_duration(asset)?;
    let version = asset.version();
    let mut manifest = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"static\" mediaPresentationDuration=\"PT{duration}S\" minBufferTime=\"PT1.5S\" profiles=\"urn:mpeg:dash:profile:isoff-main:2011\">\n  <Period start=\"PT0S\">\n"
    );
    write_video_adaptation(&mut manifest, asset, video, &version)?;
    if let Some(audio) = audio {
        write_audio_adaptation(&mut manifest, asset, audio, &version)?;
    }
    manifest.push_str("  </Period>\n</MPD>\n");
    Ok(manifest)
}

fn write_video_adaptation(
    manifest: &mut String,
    asset: &PackagedAsset,
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
    writeln!(manifest, "      <Representation id=\"video\" bandwidth=\"{}\" codecs=\"{codec}\" mimeType=\"video/mp4\" width=\"{width}\" height=\"{height}\">", estimate_bandwidth(track)?)
        .expect("writing to a String cannot fail");
    write_segment_template(manifest, asset, track, version);
    manifest.push_str("      </Representation>\n    </AdaptationSet>\n");
    Ok(())
}

fn write_audio_adaptation(
    manifest: &mut String,
    asset: &PackagedAsset,
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
    writeln!(manifest, "      <Representation id=\"audio\" bandwidth=\"{}\" codecs=\"mp4a.40.2\" mimeType=\"audio/mp4\" audioSamplingRate=\"{sample_rate}\">", estimate_bandwidth(track)?)
        .expect("writing to a String cannot fail");
    writeln!(manifest, "        <AudioChannelConfiguration schemeIdUri=\"urn:mpeg:dash:23003:3:audio_channel_configuration:2011\" value=\"{channels}\" />")
        .expect("writing to a String cannot fail");
    write_segment_template(manifest, asset, track, version);
    manifest.push_str("      </Representation>\n    </AdaptationSet>\n");
    Ok(())
}

fn write_segment_template(
    manifest: &mut String,
    asset: &PackagedAsset,
    track: &Track,
    version: &str,
) {
    writeln!(manifest, "        <SegmentTemplate timescale=\"{}\" startNumber=\"0\" initialization=\"$RepresentationID$/init.mp4?v={version}\" media=\"$RepresentationID$/segments/$Number$/media.m4s?v={version}\">", track.timescale)
        .expect("writing to a String cannot fail");
    manifest.push_str("          <SegmentTimeline>\n");
    for segment in asset.track_segments(track.id) {
        writeln!(manifest, "            <S d=\"{}\" />", segment.duration)
            .expect("writing to a String cannot fail");
    }
    manifest.push_str("          </SegmentTimeline>\n        </SegmentTemplate>\n");
}

fn presentation_duration(asset: &PackagedAsset) -> Result<String> {
    let milliseconds = asset
        .index
        .tracks
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

fn estimate_bandwidth(track: &Track) -> Result<u64> {
    let bytes = track.samples.iter().try_fold(0u64, |total, sample| {
        total
            .checked_add(u64::from(sample.size))
            .ok_or_else(|| Error::InvalidMedia("track size overflow".to_owned()))
    })?;
    bytes
        .checked_mul(8)
        .and_then(|bits| bits.checked_mul(u64::from(track.timescale)))
        .and_then(|scaled| scaled.checked_div(track.duration))
        .ok_or_else(|| Error::InvalidMedia("bandwidth calculation overflow".to_owned()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn renders_static_manifest_with_both_representations() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/h264-aac.mp4");
        let asset = PackagedAsset::load(path, 1000, &crate::config::LimitsConfig::default())
            .expect("fixture should load");

        let manifest = manifest(&asset).expect("manifest should render");

        assert!(manifest.contains("type=\"static\""));
        assert!(manifest.contains("id=\"video\""));
        assert!(manifest.contains("id=\"audio\""));
        assert!(manifest.contains("$RepresentationID$/segments/$Number$/media.m4s?v="));
        assert_eq!(manifest.matches("<S d=").count(), 6);
    }
}
