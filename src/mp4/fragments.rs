//! Sample discovery from the `moof` boxes of a fragmented file.
//!
//! A fragmented file's `moov` has empty sample tables; each fragment's `moof` lists its own
//! samples in `trun` boxes, with sizes, durations, and flags that fall back to the `tfhd` and
//! then the `trex` defaults. This turns those into the same [`Sample`] records the sample tables
//! of a progressive file produce, so nothing downstream knows the difference.

use std::collections::HashMap;

use super::boxes::{
    Reader, box_payload, child_boxes, invalid_media, optional_child, required_child,
};
use crate::config::LimitsConfig;
use crate::error::{Error, Result};
use crate::media::Sample;
use crate::source::Fragment;

/// `tfhd` flags.
const BASE_DATA_OFFSET: u32 = 0x0000_0001;
const SAMPLE_DESCRIPTION_INDEX: u32 = 0x0000_0002;
const DEFAULT_DURATION: u32 = 0x0000_0008;
const DEFAULT_SIZE: u32 = 0x0000_0010;
const DEFAULT_FLAGS: u32 = 0x0000_0020;
const DEFAULT_BASE_IS_MOOF: u32 = 0x0002_0000;

/// `trun` flags.
const DATA_OFFSET: u32 = 0x0000_0001;
const FIRST_SAMPLE_FLAGS: u32 = 0x0000_0004;
const SAMPLE_DURATION: u32 = 0x0000_0100;
const SAMPLE_SIZE: u32 = 0x0000_0200;
const SAMPLE_FLAGS: u32 = 0x0000_0400;
const SAMPLE_COMPOSITION_OFFSET: u32 = 0x0000_0800;

/// The sample flag that marks a sample as not a random-access point.
const NON_SYNC_SAMPLE: u32 = 0x0001_0000;

/// A track's fallback sample fields from `trex`, used when neither `tfhd` nor `trun` says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TrackDefaults {
    duration: u32,
    size: u32,
    flags: u32,
}

/// Reads the `trex` boxes of an `mvex` payload, by track ID.
pub(super) fn parse_defaults(mvex: &[u8]) -> Result<HashMap<u32, TrackDefaults>> {
    let mut defaults = HashMap::new();
    for trex in child_boxes(mvex)?
        .into_iter()
        .filter(|child| child.name == *b"trex")
    {
        let mut reader = Reader::new(trex.payload);
        reader.full_box()?;
        let track_id = reader.u32()?;
        // Default sample description index.
        reader.skip(4)?;
        defaults.insert(
            track_id,
            TrackDefaults {
                duration: reader.u32()?,
                size: reader.u32()?,
                flags: reader.u32()?,
            },
        );
    }
    Ok(defaults)
}

/// Every sample of `track_id`, across all fragments, in decode order.
///
/// Every traf in a `moof` is measured even when it belongs to another track, because the legacy
/// base-offset rule starts a track's data where the previous traf's ended.
pub(super) fn track_samples(
    fragments: &[Fragment],
    track_id: u32,
    defaults: TrackDefaults,
    source_len: u64,
    limits: &LimitsConfig,
) -> Result<Vec<Sample>> {
    let mut samples: Vec<Sample> = Vec::new();
    // Where the track's next sample decodes, for a fragment with no `tfdt`.
    let mut next_decode_time = 0u64;
    for fragment in fragments {
        let moof = box_payload(&fragment.bytes, 0)?;
        if moof.name != *b"moof" {
            return Err(invalid_media("fragment is not a moof box"));
        }
        // The first traf's data starts at the moof; a later one's, by the legacy rule, where
        // the previous traf's ended.
        let mut previous_end = fragment.offset;
        for traf in child_boxes(moof.payload)?
            .into_iter()
            .filter(|child| child.name == *b"traf")
        {
            let header =
                parse_track_fragment_header(required_child(traf.payload, *b"tfhd")?.payload)?;
            let base = header
                .base_data_offset
                .unwrap_or(if header.default_base_is_moof {
                    fragment.offset
                } else {
                    previous_end
                });
            let wanted = header.track_id == track_id;
            let mut decode_time = match optional_child(traf.payload, *b"tfdt")? {
                Some(tfdt) if wanted => {
                    let start = parse_decode_time(tfdt.payload)?;
                    // A gap is kept, but a fragment that starts before the previous one ended
                    // would put samples out of order, which the planner cannot cut.
                    if start < next_decode_time {
                        return Err(invalid_media(&format!(
                            "track {track_id}: a fragment starts before the previous one ended"
                        )));
                    }
                    start
                }
                _ => next_decode_time,
            };
            let mut cursor = base;
            for trun in child_boxes(traf.payload)?
                .into_iter()
                .filter(|child| child.name == *b"trun")
            {
                let run = Run {
                    header: &header,
                    defaults: if wanted {
                        defaults
                    } else {
                        TrackDefaults::default()
                    },
                    base,
                    source_len,
                    keep: wanted,
                };
                cursor = run.read(
                    trun.payload,
                    cursor,
                    &mut decode_time,
                    &mut samples,
                    limits.max_samples_per_track,
                )?;
            }
            if wanted {
                next_decode_time = decode_time;
            }
            previous_end = cursor;
        }
    }
    Ok(samples)
}

/// What a `tfhd` says.
struct TrackFragmentHeader {
    track_id: u32,
    base_data_offset: Option<u64>,
    default_base_is_moof: bool,
    default_duration: Option<u32>,
    default_size: Option<u32>,
    default_flags: Option<u32>,
}

fn parse_track_fragment_header(payload: &[u8]) -> Result<TrackFragmentHeader> {
    let mut reader = Reader::new(payload);
    let (_, flags) = reader.full_box_flags()?;
    let track_id = reader.u32()?;
    let base_data_offset = (flags & BASE_DATA_OFFSET != 0)
        .then(|| reader.u64())
        .transpose()?;
    if flags & SAMPLE_DESCRIPTION_INDEX != 0 {
        reader.skip(4)?;
    }
    let default_duration = (flags & DEFAULT_DURATION != 0)
        .then(|| reader.u32())
        .transpose()?;
    let default_size = (flags & DEFAULT_SIZE != 0)
        .then(|| reader.u32())
        .transpose()?;
    let default_flags = (flags & DEFAULT_FLAGS != 0)
        .then(|| reader.u32())
        .transpose()?;
    Ok(TrackFragmentHeader {
        track_id,
        base_data_offset,
        default_base_is_moof: flags & DEFAULT_BASE_IS_MOOF != 0,
        default_duration,
        default_size,
        default_flags,
    })
}

/// The `baseMediaDecodeTime` of a `tfdt`.
fn parse_decode_time(payload: &[u8]) -> Result<u64> {
    let mut reader = Reader::new(payload);
    let version = reader.full_box()?;
    if version == 1 {
        reader.u64()
    } else {
        reader.u32().map(u64::from)
    }
}

/// One `trun`, with everything needed to turn its entries into samples.
struct Run<'a> {
    header: &'a TrackFragmentHeader,
    defaults: TrackDefaults,
    base: u64,
    source_len: u64,
    /// False for another track's run, which is measured but not kept.
    keep: bool,
}

impl Run<'_> {
    /// Reads the run, appends its samples if they are wanted, and returns where its data ends.
    fn read(
        &self,
        payload: &[u8],
        cursor: u64,
        decode_time: &mut u64,
        samples: &mut Vec<Sample>,
        max_samples: usize,
    ) -> Result<u64> {
        let mut reader = Reader::new(payload);
        let (version, flags) = reader.full_box_flags()?;
        let count = usize::try_from(reader.u32()?)
            .map_err(|_| invalid_media("trun sample count does not fit in memory"))?;
        let mut offset = match (flags & DATA_OFFSET != 0)
            .then(|| reader.i32())
            .transpose()?
        {
            Some(data_offset) => self
                .base
                .checked_add_signed(i64::from(data_offset))
                .ok_or_else(|| invalid_media("trun data offset is out of range"))?,
            None => cursor,
        };
        let first_flags = (flags & FIRST_SAMPLE_FLAGS != 0)
            .then(|| reader.u32())
            .transpose()?;

        let entry_size = 4 * [
            SAMPLE_DURATION,
            SAMPLE_SIZE,
            SAMPLE_FLAGS,
            SAMPLE_COMPOSITION_OFFSET,
        ]
        .into_iter()
        .filter(|field| flags & field != 0)
        .count();
        // Entries take at least the bytes they declare, so the count is bounded by the box.
        if count
            .checked_mul(entry_size)
            .is_none_or(|bytes| bytes > reader.remaining())
        {
            return Err(invalid_media("trun sample count exceeds its box"));
        }
        if self.keep && samples.len().saturating_add(count) > max_samples {
            return Err(invalid_media("sample count exceeds configured limit"));
        }

        let default_duration = self
            .header
            .default_duration
            .unwrap_or(self.defaults.duration);
        let default_size = self.header.default_size.unwrap_or(self.defaults.size);
        if !self.keep && flags & SAMPLE_SIZE == 0 {
            // Another track's run whose samples all have the default size: its extent is a
            // product, and looping over a count nothing bounds would be a way to hang the parse.
            let total = u64::try_from(count)
                .ok()
                .and_then(|count| count.checked_mul(u64::from(default_size)))
                .and_then(|bytes| offset.checked_add(bytes))
                .ok_or_else(|| invalid_media("trun data extent overflows"))?;
            return Ok(total);
        }
        let default_flags = self.header.default_flags.unwrap_or(self.defaults.flags);
        for index in 0..count {
            let duration = if flags & SAMPLE_DURATION != 0 {
                reader.u32()?
            } else {
                default_duration
            };
            let size = if flags & SAMPLE_SIZE != 0 {
                reader.u32()?
            } else {
                default_size
            };
            let sample_flags = match (flags & SAMPLE_FLAGS != 0, index, first_flags) {
                (true, ..) => reader.u32()?,
                (false, 0, Some(first)) => first,
                (false, ..) => default_flags,
            };
            let composition_offset = if flags & SAMPLE_COMPOSITION_OFFSET != 0 {
                if version == 0 {
                    i32::try_from(reader.u32()?)
                        .map_err(|_| invalid_media("trun composition offset is out of range"))?
                } else {
                    reader.i32()?
                }
            } else {
                0
            };
            let end = offset
                .checked_add(u64::from(size))
                .ok_or_else(|| invalid_media("sample byte range overflow"))?;
            if self.keep {
                if end > self.source_len {
                    return Err(invalid_media("sample byte range exceeds source length"));
                }
                samples.push(Sample {
                    offset,
                    size,
                    decode_time: *decode_time,
                    duration,
                    composition_offset,
                    is_sync: sample_flags & NON_SYNC_SAMPLE == 0,
                });
            }
            *decode_time = decode_time
                .checked_add(u64::from(duration))
                .ok_or_else(|| invalid_media("decode timestamp overflow"))?;
            offset = end;
        }
        Ok(offset)
    }
}

/// Rejects a fragmented file's track that also lists samples in `moov`.
pub(super) fn reject_mixed(track_id: u32, samples_in_moov: u32) -> Result<()> {
    if samples_in_moov == 0 {
        Ok(())
    } else {
        Err(Error::Unsupported(format!(
            "track {track_id}: samples in moov as well as in fragments are not supported"
        )))
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn boxed(name: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(payload.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(&name);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn full(version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![version];
        bytes.extend_from_slice(&flags.to_be_bytes()[1..]);
        bytes.extend_from_slice(body);
        bytes
    }

    fn be(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    /// A `tfhd` with `flags` and the optional fields they announce, in order.
    fn tfhd(flags: u32, track: u32, fields: &[u8]) -> Vec<u8> {
        boxed(
            *b"tfhd",
            &full(0, flags, &[&be(&[track])[..], fields].concat()),
        )
    }

    fn tfdt(time: u32) -> Vec<u8> {
        boxed(*b"tfdt", &full(0, 0, &be(&[time])))
    }

    /// A `trun` whose entries are given as raw 32-bit words, in the order the flags announce.
    fn trun(version: u8, flags: u32, count: u32, header: &[u32], entries: &[u32]) -> Vec<u8> {
        boxed(
            *b"trun",
            &full(
                version,
                flags,
                &be(&[&[count][..], header, entries].concat()),
            ),
        )
    }

    fn traf(children: &[Vec<u8>]) -> Vec<u8> {
        boxed(*b"traf", &children.concat())
    }

    fn fragment(offset: u64, trafs: &[Vec<u8>]) -> Fragment {
        let mut payload = boxed(*b"mfhd", &full(0, 0, &be(&[1])));
        payload.extend(trafs.concat());
        Fragment {
            offset,
            bytes: Bytes::from(boxed(*b"moof", &payload)),
        }
    }

    fn samples_of(fragments: &[Fragment], track: u32, defaults: TrackDefaults) -> Vec<Sample> {
        track_samples(
            fragments,
            track,
            defaults,
            1_000_000,
            &LimitsConfig::default(),
        )
        .unwrap()
    }

    const ALL_FIELDS: u32 =
        SAMPLE_DURATION | SAMPLE_SIZE | SAMPLE_FLAGS | SAMPLE_COMPOSITION_OFFSET;

    #[test]
    fn reads_every_per_sample_field() {
        // Three samples: duration, size, flags, composition offset. The middle one is non-sync.
        let run = trun(
            0,
            DATA_OFFSET | ALL_FIELDS,
            3,
            &[100],
            &[
                10,
                500,
                0x0200_0000,
                20, //
                10,
                60,
                NON_SYNC_SAMPLE,
                0, //
                12,
                70,
                NON_SYNC_SAMPLE,
                5,
            ],
        );
        let fragments = [fragment(
            1000,
            &[traf(&[tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]), tfdt(50), run])],
        )];

        let samples = samples_of(&fragments, 1, TrackDefaults::default());

        let summary = samples
            .iter()
            .map(|s| {
                (
                    s.offset,
                    s.size,
                    s.decode_time,
                    s.duration,
                    s.composition_offset,
                    s.is_sync,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                (1100, 500, 50, 10, 20, true),
                (1600, 60, 60, 10, 0, false),
                (1660, 70, 70, 12, 5, false),
            ]
        );
    }

    #[test]
    fn fields_fall_back_from_the_run_to_the_track_fragment_header_to_trex() {
        // No per-sample fields at all: everything comes from the defaults.
        let run = trun(0, DATA_OFFSET, 2, &[0], &[]);
        let with_header_defaults = fragment(
            0,
            &[traf(&[
                tfhd(
                    DEFAULT_BASE_IS_MOOF | DEFAULT_DURATION | DEFAULT_SIZE | DEFAULT_FLAGS,
                    1,
                    &be(&[7, 40, NON_SYNC_SAMPLE]),
                ),
                run.clone(),
            ])],
        );
        let with_trex_defaults = fragment(0, &[traf(&[tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]), run])]);
        let trex = TrackDefaults {
            duration: 9,
            size: 30,
            flags: 0,
        };

        let from_header = samples_of(&[with_header_defaults], 1, trex);
        let from_trex = samples_of(&[with_trex_defaults], 1, trex);

        assert_eq!(
            from_header
                .iter()
                .map(|s| (s.duration, s.size, s.is_sync))
                .collect::<Vec<_>>(),
            [(7, 40, false), (7, 40, false)],
            "the tfhd defaults beat trex"
        );
        assert_eq!(
            from_trex
                .iter()
                .map(|s| (s.duration, s.size, s.is_sync))
                .collect::<Vec<_>>(),
            [(9, 30, true), (9, 30, true)]
        );
    }

    #[test]
    fn first_sample_flags_apply_to_the_first_sample_only() {
        // Sizes come per sample; flags come from the first-sample flags and then the default.
        let run = trun(
            0,
            DATA_OFFSET | FIRST_SAMPLE_FLAGS | SAMPLE_SIZE | SAMPLE_DURATION,
            3,
            &[0, 0x0200_0000],
            &[5, 10, 5, 20, 5, 30],
        );
        let fragments = [fragment(
            0,
            &[traf(&[
                tfhd(
                    DEFAULT_BASE_IS_MOOF | DEFAULT_FLAGS,
                    1,
                    &be(&[NON_SYNC_SAMPLE]),
                ),
                run,
            ])],
        )];

        let samples = samples_of(&fragments, 1, TrackDefaults::default());

        assert_eq!(
            samples.iter().map(|s| s.is_sync).collect::<Vec<_>>(),
            [true, false, false]
        );
    }

    #[test]
    fn version_one_composition_offsets_are_signed_and_version_zero_are_bounded() {
        let flags = DATA_OFFSET | SAMPLE_DURATION | SAMPLE_SIZE | SAMPLE_COMPOSITION_OFFSET;
        let make = |version: u8, offset: u32| {
            fragment(
                0,
                &[traf(&[
                    tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]),
                    trun(version, flags, 1, &[0], &[4, 10, offset]),
                ])],
            )
        };
        let signed = samples_of(
            &[make(1, u32::from_be_bytes((-1024i32).to_be_bytes()))],
            1,
            TrackDefaults::default(),
        );
        let too_big = track_samples(
            &[make(0, 0x8000_0000)],
            1,
            TrackDefaults::default(),
            1_000_000,
            &LimitsConfig::default(),
        );

        assert_eq!(signed[0].composition_offset, -1024);
        assert!(too_big.is_err());
    }

    #[test]
    fn runs_continue_where_the_previous_run_ended_when_they_give_no_data_offset() {
        let flags = SAMPLE_DURATION | SAMPLE_SIZE;
        let fragments = [fragment(
            200,
            &[traf(&[
                tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]),
                trun(0, flags | DATA_OFFSET, 2, &[50], &[3, 10, 3, 20]),
                trun(0, flags, 1, &[], &[3, 40]),
            ])],
        )];

        let samples = samples_of(&fragments, 1, TrackDefaults::default());

        assert_eq!(
            samples.iter().map(|s| s.offset).collect::<Vec<_>>(),
            [250, 260, 280]
        );
        assert_eq!(
            samples.iter().map(|s| s.decode_time).collect::<Vec<_>>(),
            [0, 3, 6]
        );
    }

    #[test]
    fn an_explicit_base_data_offset_beats_the_moof_position() {
        let fragments = [fragment(
            500,
            &[traf(&[
                tfhd(
                    BASE_DATA_OFFSET | DEFAULT_BASE_IS_MOOF,
                    1,
                    &9000u64.to_be_bytes(),
                ),
                trun(0, DATA_OFFSET | SAMPLE_SIZE, 1, &[16], &[8]),
            ])],
        )];

        assert_eq!(
            samples_of(&fragments, 1, TrackDefaults::default())[0].offset,
            9016
        );
    }

    #[test]
    fn without_default_base_is_moof_each_traf_starts_where_the_previous_one_ended() {
        // The legacy rule: the first traf's data starts at the moof, the second's right after the
        // first's, whichever track the first belongs to.
        let flags = SAMPLE_DURATION | SAMPLE_SIZE;
        let fragments = [fragment(
            700,
            &[
                traf(&[tfhd(0, 1, &[]), trun(0, flags, 2, &[], &[1, 100, 1, 50])]),
                traf(&[tfhd(0, 2, &[]), trun(0, flags, 1, &[], &[1, 25])]),
            ],
        )];

        let first = samples_of(&fragments, 1, TrackDefaults::default());
        let second = samples_of(&fragments, 2, TrackDefaults::default());

        assert_eq!(
            first.iter().map(|s| s.offset).collect::<Vec<_>>(),
            [700, 800]
        );
        assert_eq!(second.iter().map(|s| s.offset).collect::<Vec<_>>(), [850]);
    }

    #[test]
    fn a_missing_tfdt_continues_the_track_and_a_present_one_resets_it() {
        let flags = DATA_OFFSET | SAMPLE_DURATION | SAMPLE_SIZE;
        let run = || trun(0, flags, 2, &[0], &[10, 1, 10, 1]);
        let fragments = [
            fragment(
                0,
                &[traf(&[
                    tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]),
                    tfdt(100),
                    run(),
                ])],
            ),
            // No tfdt: continues at 120.
            fragment(50, &[traf(&[tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]), run()])]),
            // A tfdt after a gap: the gap is kept.
            fragment(
                90,
                &[traf(&[
                    tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]),
                    tfdt(500),
                    run(),
                ])],
            ),
        ];

        let samples = samples_of(&fragments, 1, TrackDefaults::default());

        assert_eq!(
            samples.iter().map(|s| s.decode_time).collect::<Vec<_>>(),
            [100, 110, 120, 130, 500, 510]
        );
    }

    #[test]
    fn a_fragment_that_starts_before_the_previous_one_ended_is_refused() {
        let run = || {
            trun(
                0,
                DATA_OFFSET | SAMPLE_DURATION | SAMPLE_SIZE,
                1,
                &[0],
                &[10, 1],
            )
        };
        let fragments = [
            fragment(
                0,
                &[traf(&[
                    tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]),
                    tfdt(100),
                    run(),
                ])],
            ),
            // The first ended at 110; this one claims to start at 105.
            fragment(
                50,
                &[traf(&[
                    tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]),
                    tfdt(105),
                    run(),
                ])],
            ),
        ];

        let error = track_samples(
            &fragments,
            1,
            TrackDefaults::default(),
            1_000_000,
            &LimitsConfig::default(),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("before the previous one ended"),
            "{error}"
        );
    }

    #[test]
    fn a_run_of_another_track_with_no_entry_bytes_cannot_hang_the_parse() {
        // Four billion samples of the default size, in a track nobody asked for: the extent is a
        // product, and iterating would take minutes.
        let hostile = trun(0, DATA_OFFSET, u32::MAX, &[0], &[]);
        let fragments = [fragment(
            0,
            &[
                traf(&[
                    tfhd(DEFAULT_BASE_IS_MOOF | DEFAULT_SIZE, 2, &be(&[1])),
                    hostile,
                ]),
                traf(&[
                    tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]),
                    trun(0, DATA_OFFSET | SAMPLE_SIZE, 1, &[0], &[8]),
                ]),
            ],
        )];

        let samples = samples_of(&fragments, 1, TrackDefaults::default());

        assert_eq!(samples.len(), 1);
    }

    #[test]
    fn the_wanted_tracks_own_count_is_bounded_before_expansion() {
        let hostile = trun(0, DATA_OFFSET, u32::MAX, &[0], &[]);
        let fragments = [fragment(
            0,
            &[traf(&[
                tfhd(DEFAULT_BASE_IS_MOOF | DEFAULT_SIZE, 1, &be(&[1])),
                hostile,
            ])],
        )];

        let error = track_samples(
            &fragments,
            1,
            TrackDefaults::default(),
            u64::MAX,
            &LimitsConfig::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("sample count"), "{error}");
    }

    #[test]
    fn a_run_cannot_claim_more_entries_than_it_holds() {
        let run = trun(0, DATA_OFFSET | ALL_FIELDS, 1000, &[0], &[1, 2, 3, 4]);
        let fragments = [fragment(
            0,
            &[traf(&[tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]), run])],
        )];

        assert!(
            track_samples(
                &fragments,
                1,
                TrackDefaults::default(),
                1_000_000,
                &LimitsConfig::default()
            )
            .is_err()
        );
    }

    #[test]
    fn samples_outside_the_source_are_rejected() {
        let fragments = [fragment(
            0,
            &[traf(&[
                tfhd(DEFAULT_BASE_IS_MOOF, 1, &[]),
                trun(0, DATA_OFFSET | SAMPLE_SIZE, 1, &[900], &[200]),
            ])],
        )];

        let result = track_samples(
            &fragments,
            1,
            TrackDefaults::default(),
            1000,
            &LimitsConfig::default(),
        );

        assert!(result.is_err(), "900 + 200 is past a 1000-byte source");
    }

    #[test]
    fn trex_defaults_are_read_by_track() {
        let trex = |track: u32, duration: u32, size: u32, flags: u32| {
            boxed(
                *b"trex",
                &full(0, 0, &be(&[track, 1, duration, size, flags])),
            )
        };
        let mvex = [trex(1, 3, 4, 5), trex(2, 6, 7, 8)].concat();

        let defaults = parse_defaults(&mvex).unwrap();

        assert_eq!(
            defaults[&2],
            TrackDefaults {
                duration: 6,
                size: 7,
                flags: 8
            }
        );
        assert_eq!(defaults.len(), 2);
    }

    #[test]
    fn samples_in_moov_next_to_fragments_are_refused() {
        assert!(reject_mixed(1, 0).is_ok());
        let error = reject_mixed(3, 12).unwrap_err();
        assert!(error.to_string().contains("track 3"), "{error}");
    }
}
