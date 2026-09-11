#![no_main]
#![allow(dead_code)]
#![allow(unused_imports)]

use std::io::Write;

use libfuzzer_sys::fuzz_target;

#[path = "../../src/config.rs"]
mod config;
#[path = "../../src/error.rs"]
mod error;
#[path = "../../src/fmp4/mod.rs"]
mod fmp4;
#[path = "../../src/media/mod.rs"]
mod media;
#[path = "../../src/mp4/mod.rs"]
mod mp4;
#[path = "../../src/segment/mod.rs"]
mod segment;
#[path = "../../src/source/mod.rs"]
mod source;

use config::LimitsConfig;
use source::LocalMediaSource;

fuzz_target!(|data: &[u8]| {
    let limits = LimitsConfig::default();
    if data.is_empty() || data.len() as u64 > limits.max_metadata_bytes {
        return;
    }

    let mut file = tempfile::NamedTempFile::new().expect("temporary file should be available");
    file.write_all(data)
        .expect("temporary fuzz input should be writable");
    let Ok(source) = LocalMediaSource::open(file.path()) else {
        return;
    };
    let Ok(index) = mp4::parse(&source, &limits) else {
        return;
    };
    let Ok(plan) = segment::plan(&index, 6000, &limits) else {
        return;
    };

    for track in &index.tracks {
        let _ = fmp4::write_init_segment(&source, track.id);
        for segment in plan.segments.iter().take(2) {
            let Some(track_segment) = segment
                .tracks
                .iter()
                .find(|candidate| candidate.track_id == track.id)
                .copied()
            else {
                continue;
            };
            let _ = fmp4::prepare_media_segment(
                track,
                track_segment,
                segment.index.saturating_add(1),
                &limits,
            );
        }
    }
});
