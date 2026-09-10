use std::fmt::Write;

use crate::asset::PackagedAsset;
use crate::error::{Error, Result};
use crate::media::{CodecConfig, Track, TrackKind};

pub(crate) fn master_playlist(asset: &PackagedAsset) -> Result<String> {
    let video = asset.track(TrackKind::Video)?;
    let audio = asset.track(TrackKind::Audio).ok();
    let CodecConfig::Avc {
        width,
        height,
        profile,
        compatibility,
        level,
        ..
    } = video.codec
    else {
        return Err(Error::Unsupported("HLS video must be H.264"));
    };
    let mut codecs = format!("avc1.{profile:02x}{compatibility:02x}{level:02x}");
    let version = asset.version();
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    let audio_attribute = if audio.is_some() {
        codecs.push_str(",mp4a.40.2");
        playlist.push_str(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio/index.m3u8?v={version}\"\n",
        );
        ",AUDIO=\"audio\""
    } else {
        ""
    };
    let bandwidth = estimate_bandwidth(video)?
        .checked_add(audio.map(estimate_bandwidth).transpose()?.unwrap_or(0))
        .ok_or_else(|| Error::InvalidMedia("bandwidth overflow".to_owned()))?;
    writeln!(
        playlist,
        "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},CODECS=\"{codecs}\",RESOLUTION={width}x{height}{audio_attribute}"
    )
    .expect("writing to a String cannot fail");
    writeln!(playlist, "video/index.m3u8?v={version}").expect("writing to a String cannot fail");
    Ok(playlist)
}

pub(crate) fn media_playlist(asset: &PackagedAsset, kind: TrackKind) -> Result<String> {
    let track = asset.track(kind)?;
    let version = asset.version();
    let segments = asset.track_segments(track.id).collect::<Vec<_>>();
    let target_duration = segments
        .iter()
        .map(|segment| div_ceil(segment.duration, u64::from(track.timescale)))
        .max()
        .unwrap_or(1);
    let mut playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target_duration}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init.mp4?v={version}\"\n"
    );
    for (index, segment) in segments.iter().enumerate() {
        let milliseconds = segment
            .duration
            .checked_mul(1000)
            .and_then(|duration| duration.checked_div(u64::from(track.timescale)))
            .ok_or_else(|| Error::InvalidMedia("segment duration overflow".to_owned()))?;
        writeln!(
            playlist,
            "#EXTINF:{}.{:03},\nsegments/{index}/media.m4s?v={version}",
            milliseconds / 1000,
            milliseconds % 1000
        )
        .expect("writing to a String cannot fail");
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    Ok(playlist)
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

const fn div_ceil(dividend: u64, divisor: u64) -> u64 {
    if dividend % divisor == 0 {
        dividend / divisor
    } else {
        dividend / divisor + 1
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn asset() -> PackagedAsset {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/h264-aac.mp4");
        PackagedAsset::load(path, 1000, &crate::config::LimitsConfig::default())
            .expect("fixture should load")
    }

    #[test]
    fn master_references_separate_audio_and_video_playlists() {
        let playlist = master_playlist(&asset()).expect("master playlist should render");

        assert!(playlist.contains("CODECS=\"avc1.64000d,mp4a.40.2\""));
        assert!(playlist.contains("URI=\"audio/index.m3u8?v="));
        assert!(playlist.contains("video/index.m3u8?v="));
    }

    #[test]
    fn video_playlist_references_init_and_three_segments() {
        let playlist =
            media_playlist(&asset(), TrackKind::Video).expect("media playlist should render");

        assert!(playlist.contains("#EXT-X-TARGETDURATION:1"));
        assert!(playlist.contains("#EXT-X-MAP:URI=\"init.mp4?v="));
        assert_eq!(playlist.matches("#EXTINF:1.000,").count(), 3);
        assert!(playlist.contains("segments/2/media.m4s?v="));
        assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
    }
}
