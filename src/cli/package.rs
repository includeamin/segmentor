//! The `package` command: write init and media segments to disk.
//!
//! A development and test tool that runs the same packaging code as the server, but assembles
//! each segment in memory and writes it as a file.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::{CodecConfig, TrackKind};
use crate::source::LocalMediaSource;
use crate::{fmp4, mp4, segment};

pub(super) fn run(arguments: impl Iterator<Item = OsString>) -> Result<()> {
    let options = PackageOptions::parse(arguments)?;
    package(&options)
}

#[derive(Debug)]
struct PackageOptions {
    input: PathBuf,
    output: PathBuf,
    segment_duration_ms: u64,
}

impl PackageOptions {
    fn parse(mut arguments: impl Iterator<Item = OsString>) -> Result<Self> {
        let mut input = None;
        let mut output = None;
        let mut segment_duration_ms = 1000u64;
        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("--input") => input = arguments.next().map(PathBuf::from),
                Some("--output") => output = arguments.next().map(PathBuf::from),
                Some("--segment-duration-ms") => {
                    let value = arguments.next().ok_or_else(|| {
                        Error::InvalidMedia("missing segment duration value".to_owned())
                    })?;
                    segment_duration_ms = value
                        .to_str()
                        .ok_or_else(|| {
                            Error::InvalidMedia("segment duration must be UTF-8".to_owned())
                        })?
                        .parse()
                        .map_err(|_| {
                            Error::InvalidMedia(
                                "segment duration must be a positive integer".to_owned(),
                            )
                        })?;
                }
                _ => return Err(Error::InvalidMedia("unknown package argument".to_owned())),
            }
        }

        Ok(Self {
            input: input.ok_or_else(|| Error::InvalidMedia("missing --input".to_owned()))?,
            output: output.ok_or_else(|| Error::InvalidMedia("missing --output".to_owned()))?,
            segment_duration_ms,
        })
    }
}

fn package(options: &PackageOptions) -> Result<()> {
    let source = LocalMediaSource::open(&options.input)?;
    let limits = LimitsConfig::default();
    let index = mp4::parse(&source, &limits)?;
    let plan = segment::plan(&index, options.segment_duration_ms, &limits)?;
    std::fs::create_dir_all(&options.output)?;

    for track in &index.tracks {
        let label = match track.kind {
            TrackKind::Audio => "audio",
            TrackKind::Video => "video",
        };
        let init = fmp4::write_init_segment(&source, track.id)?;
        std::fs::write(options.output.join(format!("{label}-init.mp4")), init)?;

        for segment in &plan.segments {
            let track_segment = segment
                .tracks
                .iter()
                .find(|candidate| candidate.track_id == track.id)
                .copied()
                .ok_or_else(|| Error::InvalidMedia("segment is missing a track".to_owned()))?;
            let sequence_number = segment
                .index
                .checked_add(1)
                .ok_or_else(|| Error::InvalidMedia("sequence number overflow".to_owned()))?;
            let media =
                fmp4::write_media_segment(&source, track, track_segment, sequence_number, &limits)?;
            std::fs::write(
                options
                    .output
                    .join(format!("{label}-{}.m4s", segment.index)),
                media,
            )?;
        }
    }

    let identity = &index.source;
    println!(
        "packaged {} bytes from {} (device {}, inode {}, mtime {}.{:09})",
        identity.length,
        identity.canonical_path.display(),
        identity.device,
        identity.inode,
        identity.modified_seconds,
        identity.modified_nanoseconds
    );
    println!(
        "movie: timescale {}, duration {}, segments {}",
        index.movie_timescale,
        index.duration,
        plan.segments.len()
    );
    for track in &index.tracks {
        let codec = match &track.codec {
            CodecConfig::Aac {
                sample_rate,
                channels,
            } => format!("AAC-LC {sample_rate} Hz {channels} channel(s)"),
            CodecConfig::Avc {
                width,
                height,
                sequence_parameter_set,
                picture_parameter_set,
                ..
            } => format!(
                "H.264 {width}x{height} (SPS {} bytes, PPS {} bytes)",
                sequence_parameter_set.len(),
                picture_parameter_set.len()
            ),
        };
        println!(
            "track {}: {codec}, timescale {}, duration {}, samples {}",
            track.id,
            track.timescale,
            track.duration,
            track.samples.len()
        );
    }
    Ok(())
}
