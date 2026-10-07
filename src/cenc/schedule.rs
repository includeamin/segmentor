//! Where each key period of a rotating asset begins, in segments (TDD 0013).

use super::keys::Encryption;
use crate::error::{Error, Result};
use crate::media::Track;
use crate::segment::SegmentPlan;

/// One stretch of the timeline and the keys it is encrypted under.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScheduledPeriod<'a> {
    /// The first segment of the period, whose index is the same for every track.
    pub(crate) first_segment: u32,
    /// `None` is a clear period.
    pub(crate) encryption: Option<&'a Encryption>,
}

/// The periods of `encryption` placed on `plan`. A period begins at the first segment that starts
/// at or after its `start_ms`; one that begins after the last segment is dropped, and two that
/// would begin at the same segment are an error, since the mapper asked for a key that no
/// segment would use. Without rotation this is one period from segment 0.
pub(crate) fn schedule<'a>(
    encryption: &'a Encryption,
    tracks: &[Track],
    plan: &SegmentPlan,
) -> Result<Vec<ScheduledPeriod<'a>>> {
    if !encryption.is_rotating() {
        return Ok(vec![ScheduledPeriod {
            first_segment: 0,
            encryption: Some(encryption),
        }]);
    }
    // Segments are cut by the first track of each (the reference), so its clock places them.
    let start_of = |segment: &crate::segment::Segment| -> Option<(u64, u32)> {
        let part = segment.tracks.first()?;
        let track = tracks.iter().find(|track| track.id == part.track_id)?;
        Some((part.decode_time, track.timescale))
    };
    let base = plan
        .segments
        .first()
        .and_then(start_of)
        .map_or(0, |(time, _)| time);
    let mut periods: Vec<ScheduledPeriod<'a>> = Vec::with_capacity(encryption.periods.len());
    for period in &encryption.periods {
        let first = plan.segments.iter().find(|segment| {
            start_of(segment).is_some_and(|(time, timescale)| {
                u128::from(time.saturating_sub(base)) * 1000
                    >= u128::from(period.start_ms) * u128::from(timescale)
            })
        });
        let Some(first) = first else {
            continue;
        };
        if periods
            .last()
            .is_some_and(|previous| previous.first_segment >= first.index)
        {
            return Err(Error::InvalidMedia(
                "two encryption periods begin in the same segment".to_owned(),
            ));
        }
        periods.push(ScheduledPeriod {
            first_segment: first.index,
            encryption: period.encryption.as_deref(),
        });
    }
    Ok(periods)
}
