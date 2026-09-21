use std::fmt;

use crate::source::SourceIdentity;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaIndex {
    pub(crate) source: SourceIdentity,
    pub(crate) movie_timescale: u32,
    pub(crate) duration: u64,
    pub(crate) tracks: Vec<Track>,
    /// Tracks in the file that are not packaged, with the reason, so the registry can log them.
    pub(crate) skipped_tracks: Vec<SkippedTrack>,
    /// How a fragmented source's fragments were found; `None` for a progressive file.
    pub(crate) fragmentation: Option<Fragmentation>,
}

/// What discovering a fragmented source's fragments involved, for the operator's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fragmentation {
    pub(crate) fragments: usize,
    pub(crate) discovery: crate::source::Discovery,
    /// What was left out because the file ends partway through a fragment.
    pub(crate) dropped_tail: Option<crate::source::DroppedTail>,
}

/// A track present in the source but left out of the packaged asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedTrack {
    pub(crate) id: u32,
    /// The four-character handler type, such as `tmcd` or `meta`.
    pub(crate) handler: String,
    pub(crate) reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Track {
    pub(crate) id: u32,
    /// The name this track has in URLs: `video`, `audio-1`, `audio-2`, and so on.
    pub(crate) key: TrackKey,
    pub(crate) kind: TrackKind,
    /// ISO 639-2/T language from `mdhd`; `und` when the file does not say.
    pub(crate) language: String,
    /// Ticks added to every sample's decode time so the edit list never needs a negative
    /// timestamp. Zero for files without an edit list.
    pub(crate) timeline_shift: u64,
    pub(crate) timescale: u32,
    pub(crate) duration: u64,
    pub(crate) codec: CodecConfig,
    pub(crate) samples: Vec<Sample>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TrackKind {
    Audio,
    Video,
}

/// A track's name in URLs. There is one video track, and audio tracks are numbered from one in
/// file order, so `audio-1` is the first audio track even when the file has only one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TrackKey {
    pub(crate) kind: TrackKind,
    ordinal: u16,
}

impl TrackKey {
    pub(crate) const VIDEO: Self = Self {
        kind: TrackKind::Video,
        ordinal: 1,
    };

    /// The key of the `ordinal`th audio track, counting from one.
    pub(crate) const fn audio(ordinal: u16) -> Self {
        Self {
            kind: TrackKind::Audio,
            ordinal,
        }
    }

    /// Parses the canonical spelling only, so one track has exactly one URL.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        if text == "video" {
            return Some(Self::VIDEO);
        }
        let ordinal: u16 = text.strip_prefix("audio-")?.parse().ok()?;
        let key = Self::audio(ordinal);
        (ordinal != 0 && key.to_string() == text).then_some(key)
    }
}

impl fmt::Display for TrackKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            TrackKind::Video => formatter.write_str("video"),
            TrackKind::Audio => write!(formatter, "audio-{}", self.ordinal),
        }
    }
}

/// What packaging needs to know about a track's codec: enough to write the manifest's codec
/// string and dimensions. The sample entry itself is copied into the init segment untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodecConfig {
    Avc {
        width: u16,
        height: u16,
        profile: u8,
        compatibility: u8,
        level: u8,
        sequence_parameter_set: Vec<u8>,
        picture_parameter_set: Vec<u8>,
    },
    /// HEVC, with the codec string already derived from `hvcC`.
    Hevc {
        width: u16,
        height: u16,
        codecs: String,
    },
    Vp9 {
        width: u16,
        height: u16,
        codecs: String,
    },
    Av1 {
        width: u16,
        height: u16,
        codecs: String,
    },
    /// AAC-LC (object type 2), HE-AAC (5), or HE-AACv2 (29).
    Aac {
        sample_rate: u32,
        channels: u16,
        object_type: u8,
    },
    Ac3 {
        sample_rate: u32,
        channels: u16,
    },
    Eac3 {
        sample_rate: u32,
        channels: u16,
    },
    Opus {
        channels: u16,
    },
    Flac {
        sample_rate: u32,
        channels: u16,
    },
}

impl CodecConfig {
    /// The RFC 6381 codec string for HLS `CODECS` and DASH `codecs`.
    pub(crate) fn codecs(&self) -> String {
        match self {
            Self::Avc {
                profile,
                compatibility,
                level,
                ..
            } => format!("avc1.{profile:02x}{compatibility:02x}{level:02x}"),
            Self::Hevc { codecs, .. } | Self::Vp9 { codecs, .. } | Self::Av1 { codecs, .. } => {
                codecs.clone()
            }
            Self::Aac { object_type, .. } => format!("mp4a.40.{object_type}"),
            Self::Ac3 { .. } => "ac-3".to_owned(),
            Self::Eac3 { .. } => "ec-3".to_owned(),
            Self::Opus { .. } => "opus".to_owned(),
            Self::Flac { .. } => "fLaC".to_owned(),
        }
    }

    /// Width and height in pixels, for video codecs.
    pub(crate) const fn dimensions(&self) -> Option<(u16, u16)> {
        match self {
            Self::Avc { width, height, .. }
            | Self::Hevc { width, height, .. }
            | Self::Vp9 { width, height, .. }
            | Self::Av1 { width, height, .. } => Some((*width, *height)),
            _ => None,
        }
    }

    /// Sample rate in hertz and channel count, for audio codecs.
    pub(crate) const fn audio_format(&self) -> Option<(u32, u16)> {
        match self {
            Self::Aac {
                sample_rate,
                channels,
                ..
            }
            | Self::Ac3 {
                sample_rate,
                channels,
            }
            | Self::Eac3 {
                sample_rate,
                channels,
            }
            | Self::Flac {
                sample_rate,
                channels,
            } => Some((*sample_rate, *channels)),
            // Opus always decodes at 48 kHz, whatever rate the source was captured at.
            Self::Opus { channels } => Some((48_000, *channels)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sample {
    pub(crate) offset: u64,
    pub(crate) size: u32,
    pub(crate) decode_time: u64,
    pub(crate) duration: u32,
    pub(crate) composition_offset: i32,
    pub(crate) is_sync: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_keys_round_trip_through_their_canonical_spelling() {
        for key in [TrackKey::VIDEO, TrackKey::audio(1), TrackKey::audio(12)] {
            assert_eq!(TrackKey::parse(&key.to_string()), Some(key));
        }
    }

    #[test]
    fn track_keys_reject_every_non_canonical_spelling() {
        for text in [
            "audio",
            "audio-",
            "audio-0",
            "audio-01",
            "audio-+1",
            "audio-1 ",
            "Video",
            "video-1",
            "",
            "audio-99999",
        ] {
            assert_eq!(TrackKey::parse(text), None, "{text:?}");
        }
    }
}
