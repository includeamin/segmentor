use std::fmt::Write;

use super::Presentation;
use crate::error::{Error, Result};
use crate::media::{CodecConfig, TrackKind};

pub(crate) fn master_playlist(presentation: Presentation<'_>) -> Result<String> {
    let video = presentation.track(TrackKind::Video)?;
    let audio = presentation.track(TrackKind::Audio).ok();
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
    let version = presentation.version();
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    let audio_attribute = if audio.is_some() {
        codecs.push_str(",mp4a.40.2");
        writeln!(
            playlist,
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio/index.m3u8?v={version}\""
        )
        .expect("writing to a String cannot fail");
        ",AUDIO=\"audio\""
    } else {
        ""
    };
    let video_bandwidth = presentation.bandwidth(video)?;
    let audio_bandwidth = audio
        .map(|track| presentation.bandwidth(track))
        .transpose()?;
    let bandwidth = video_bandwidth
        .peak
        .checked_add(audio_bandwidth.map_or(0, |audio| audio.peak))
        .ok_or_else(|| Error::InvalidMedia("bandwidth overflow".to_owned()))?;
    let average_bandwidth = video_bandwidth
        .average
        .checked_add(audio_bandwidth.map_or(0, |audio| audio.average))
        .ok_or_else(|| Error::InvalidMedia("bandwidth overflow".to_owned()))?;
    writeln!(
        playlist,
        "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},AVERAGE-BANDWIDTH={average_bandwidth},CODECS=\"{codecs}\",RESOLUTION={width}x{height}{audio_attribute}"
    )
    .expect("writing to a String cannot fail");
    writeln!(playlist, "video/index.m3u8?v={version}").expect("writing to a String cannot fail");
    Ok(playlist)
}

pub(crate) fn media_playlist(presentation: Presentation<'_>, kind: TrackKind) -> Result<String> {
    let track = presentation.track(kind)?;
    let version = presentation.version();
    let segments = presentation.track_segments(track.id).collect::<Vec<_>>();
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

const fn div_ceil(dividend: u64, divisor: u64) -> u64 {
    if dividend % divisor == 0 {
        dividend / divisor
    } else {
        dividend / divisor + 1
    }
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
        assert!(playlist.contains(&format!("URI=\"audio/index.m3u8?v={version}\"")));
        assert!(playlist.contains(&format!("video/index.m3u8?v={version}\n")));
        assert!(
            !playlist.contains('{'),
            "no unexpanded placeholders: {playlist}"
        );
    }

    #[test]
    fn video_playlist_references_init_and_three_segments() {
        let loaded = Loaded::h264_aac();
        let playlist = media_playlist(loaded.presentation(), TrackKind::Video)
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

        let error = media_playlist(presentation, TrackKind::Audio)
            .expect_err("audio is absent from the view");

        assert!(matches!(error, Error::NotFound(_)));
    }
}
