use ::mp4::{Mp4Reader, WriteBox};

use crate::error::{Error, Result};
use crate::source::SparseFile;

pub(crate) fn write_init_segment(metadata: &SparseFile, track_id: u32) -> Result<Vec<u8>> {
    let reader = Mp4Reader::read_header(metadata.reader(), metadata.len())?;
    let mut movie = reader.moov.clone();
    movie.traks.retain(|track| track.tkhd.track_id == track_id);
    if movie.traks.len() != 1 {
        return Err(Error::InvalidMedia(format!(
            "track {track_id} does not exist"
        )));
    }

    movie.mvhd.duration = 0;
    movie.udta = None;
    let track = &mut movie.traks[0];
    track.tkhd.duration = 0;
    // The edit list was applied to the sample timestamps when the file was parsed; keeping it
    // here would make a player apply it a second time.
    track.edts = None;
    track.mdia.mdhd.duration = 0;
    let sample_table = &mut track.mdia.minf.stbl;
    sample_table.stts.entries.clear();
    sample_table.ctts = None;
    sample_table.stss = None;
    sample_table.stsc.entries.clear();
    sample_table.stsz.sample_size = 0;
    sample_table.stsz.sample_count = 0;
    sample_table.stsz.sample_sizes.clear();
    if let Some(offsets) = &mut sample_table.stco {
        offsets.entries.clear();
    }
    if let Some(offsets) = &mut sample_table.co64 {
        offsets.entries.clear();
    }
    movie.mvex = None;

    let mut output = Vec::new();
    reader.ftyp.write_box(&mut output)?;
    let mut movie_bytes = Vec::new();
    movie.write_box(&mut movie_bytes)?;
    let movie_extends = movie_extends_box(track_id);
    let movie_size = movie_bytes
        .len()
        .checked_add(movie_extends.len())
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| Error::InvalidMedia("initialization segment is too large".to_owned()))?;
    movie_bytes[..4].copy_from_slice(&movie_size.to_be_bytes());
    movie_bytes.extend_from_slice(&movie_extends);
    output.extend_from_slice(&movie_bytes);
    Ok(output)
}

fn movie_extends_box(track_id: u32) -> Vec<u8> {
    let mut track_extends = Vec::with_capacity(32);
    track_extends.extend_from_slice(&32u32.to_be_bytes());
    track_extends.extend_from_slice(b"trex");
    track_extends.extend_from_slice(&0u32.to_be_bytes());
    track_extends.extend_from_slice(&track_id.to_be_bytes());
    track_extends.extend_from_slice(&1u32.to_be_bytes());
    track_extends.extend_from_slice(&0u32.to_be_bytes());
    track_extends.extend_from_slice(&0u32.to_be_bytes());
    track_extends.extend_from_slice(&0u32.to_be_bytes());

    let mut movie_extends = Vec::with_capacity(40);
    movie_extends.extend_from_slice(&40u32.to_be_bytes());
    movie_extends.extend_from_slice(b"mvex");
    movie_extends.extend_from_slice(&track_extends);
    movie_extends
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::LimitsConfig;
    use crate::media::TrackKey;
    use crate::mp4;
    use crate::source::{LocalMediaSource, MediaSourceKind};

    fn parsed(name: &str) -> mp4::ParsedMedia {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let source = MediaSourceKind::Local(std::sync::Arc::new(
            LocalMediaSource::open(path).expect("fixture should open"),
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build")
            .block_on(mp4::parse(&source, &LimitsConfig::default()))
            .expect("fixture should parse")
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn the_init_segment_does_not_repeat_the_edit_list_the_timestamps_already_apply() {
        let media = parsed("h264-aac-default-edits.mp4");
        assert!(
            contains(media.metadata.moov_bytes(), b"elst"),
            "the fixture must have an edit list for this test to mean anything"
        );

        for track in &media.index.tracks {
            let init = write_init_segment(&media.metadata, track.id).unwrap();
            assert!(
                !contains(&init, b"edts") && !contains(&init, b"elst"),
                "{} init still carries an edit list",
                track.key
            );
        }
        assert_eq!(media.index.tracks[0].key, TrackKey::VIDEO);
    }
}
