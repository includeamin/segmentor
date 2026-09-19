//! Entry points for the fuzz target.
//!
//! Hidden from generated documentation: this exists so `fuzz/` can depend on the library instead
//! of including source files by path.

use std::path::Path;

use crate::config::LimitsConfig;
use crate::source::LocalMediaSource;
use crate::{fmp4, mp4, segment};

/// Drives the whole packaging pipeline over the file at `path`: parse, plan, then write init
/// segments and prepare the first two media segments of every track.
///
/// Every failure is expected for arbitrary input and is ignored; the fuzzer is looking for
/// panics, hangs, and runaway allocation.
#[doc(hidden)]
pub fn exercise_media_pipeline(path: &Path) {
    let limits = LimitsConfig::default();
    let Ok(source) = LocalMediaSource::open(path) else {
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
}

/// The largest input the fuzz target should hand to [`exercise_media_pipeline`].
#[doc(hidden)]
#[must_use]
pub fn max_input_bytes() -> u64 {
    LimitsConfig::default().max_metadata_bytes
}
