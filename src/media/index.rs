use crate::source::SourceIdentity;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaIndex {
    pub(crate) source: SourceIdentity,
    pub(crate) movie_timescale: u32,
    pub(crate) duration: u64,
    pub(crate) tracks: Vec<Track>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Track {
    pub(crate) id: u32,
    pub(crate) kind: TrackKind,
    pub(crate) timescale: u32,
    pub(crate) duration: u64,
    pub(crate) codec: CodecConfig,
    pub(crate) samples: Vec<Sample>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackKind {
    Audio,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodecConfig {
    Aac {
        sample_rate: u32,
        channels: u16,
    },
    Avc {
        width: u16,
        height: u16,
        sequence_parameter_set: Vec<u8>,
        picture_parameter_set: Vec<u8>,
    },
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
