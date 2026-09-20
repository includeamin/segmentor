//! Sample tables (`stbl`): parsing the boxes, then expanding them into per-sample records.
//!
//! Tables are run-length encoded, so a few bytes can describe billions of samples. Every count
//! is checked against the box that holds it before anything is allocated, and expansion is
//! checked against the configured sample limit before it begins.

use std::collections::HashSet;

use super::boxes::{Reader, invalid_media, optional_child, required_child};
use crate::config::LimitsConfig;
use crate::error::Result;
use crate::media::Sample;

/// The raw sample tables of one track.
#[derive(Debug)]
pub(super) struct SampleTables {
    /// `(sample_count, sample_delta)` runs from `stts`.
    time_to_sample: Vec<(u32, u32)>,
    /// `(sample_count, sample_offset)` runs from `ctts`, if the track reorders samples.
    composition: Option<Vec<(u32, i32)>>,
    /// One-based sample numbers of the random-access samples from `stss`; absent means all.
    sync: Option<Vec<u32>>,
    /// `(first_chunk, samples_per_chunk)` runs from `stsc`.
    sample_to_chunk: Vec<(u32, u32)>,
    sizes: SampleSizes,
    chunk_offsets: Vec<u64>,
}

#[derive(Debug)]
enum SampleSizes {
    /// Every sample has the same size, so no table exists.
    Constant {
        size: u32,
        count: u32,
    },
    Table(Vec<u32>),
}

impl SampleSizes {
    fn count(&self) -> usize {
        match self {
            Self::Constant { count, .. } => usize::try_from(*count).unwrap_or(usize::MAX),
            Self::Table(sizes) => sizes.len(),
        }
    }
}

/// Reads every table `stbl` holds that packaging needs.
pub(super) fn parse_sample_tables(stbl: &[u8]) -> Result<SampleTables> {
    let chunk_offsets = if let Some(stco) = optional_child(stbl, *b"stco")? {
        parse_chunk_offsets(stco.payload, false)?
    } else if let Some(co64) = optional_child(stbl, *b"co64")? {
        parse_chunk_offsets(co64.payload, true)?
    } else {
        return Err(invalid_media("missing stco/co64 chunk offsets"));
    };
    let sizes = if let Some(stsz) = optional_child(stbl, *b"stsz")? {
        parse_stsz(stsz.payload)?
    } else if let Some(stz2) = optional_child(stbl, *b"stz2")? {
        parse_stz2(stz2.payload)?
    } else {
        return Err(invalid_media("missing stsz/stz2 sample sizes"));
    };
    Ok(SampleTables {
        time_to_sample: parse_runs(required_child(stbl, *b"stts")?.payload)?,
        composition: optional_child(stbl, *b"ctts")?
            .map(|ctts| parse_ctts(ctts.payload))
            .transpose()?,
        sync: optional_child(stbl, *b"stss")?
            .map(|stss| parse_stss(stss.payload))
            .transpose()?,
        sample_to_chunk: parse_stsc(required_child(stbl, *b"stsc")?.payload)?,
        sizes,
        chunk_offsets,
    })
}

/// `stts` layout: `(count, delta)` pairs.
fn parse_runs(payload: &[u8]) -> Result<Vec<(u32, u32)>> {
    let mut reader = Reader::new(payload);
    reader.full_box()?;
    let count = reader.entry_count(8)?;
    (0..count)
        .map(|_| Ok((reader.u32()?, reader.u32()?)))
        .collect()
}

/// `ctts` layout: `(count, offset)` pairs; version 0 offsets are unsigned, version 1 signed.
fn parse_ctts(payload: &[u8]) -> Result<Vec<(u32, i32)>> {
    let mut reader = Reader::new(payload);
    let version = reader.full_box()?;
    let count = reader.entry_count(8)?;
    (0..count)
        .map(|_| {
            let run = reader.u32()?;
            let offset = if version == 0 {
                i32::try_from(reader.u32()?)
                    .map_err(|_| invalid_media("ctts version 0 offset is out of range"))?
            } else {
                reader.i32()?
            };
            Ok((run, offset))
        })
        .collect()
}

fn parse_stss(payload: &[u8]) -> Result<Vec<u32>> {
    let mut reader = Reader::new(payload);
    reader.full_box()?;
    let count = reader.entry_count(4)?;
    (0..count).map(|_| reader.u32()).collect()
}

/// `stsc` layout: `(first_chunk, samples_per_chunk, sample_description_index)` triples.
fn parse_stsc(payload: &[u8]) -> Result<Vec<(u32, u32)>> {
    let mut reader = Reader::new(payload);
    reader.full_box()?;
    let count = reader.entry_count(12)?;
    (0..count)
        .map(|_| {
            let first_chunk = reader.u32()?;
            let samples_per_chunk = reader.u32()?;
            reader.skip(4)?;
            Ok((first_chunk, samples_per_chunk))
        })
        .collect()
}

fn parse_stsz(payload: &[u8]) -> Result<SampleSizes> {
    let mut reader = Reader::new(payload);
    reader.full_box()?;
    let size = reader.u32()?;
    let count = reader.u32()?;
    if size != 0 {
        return Ok(SampleSizes::Constant { size, count });
    }
    let entries = usize::try_from(count)
        .ok()
        .filter(|entries| {
            entries
                .checked_mul(4)
                .is_some_and(|bytes| bytes <= reader.remaining())
        })
        .ok_or_else(|| invalid_media("stsz entry count exceeds its box"))?;
    Ok(SampleSizes::Table(
        (0..entries).map(|_| reader.u32()).collect::<Result<_>>()?,
    ))
}

/// `stz2`: the compact form, with 4-, 8-, or 16-bit sizes.
fn parse_stz2(payload: &[u8]) -> Result<SampleSizes> {
    let mut reader = Reader::new(payload);
    reader.full_box()?;
    reader.skip(3)?;
    let field_bits = reader.u8()?;
    let count = usize::try_from(reader.u32()?)
        .map_err(|_| invalid_media("stz2 sample count does not fit in memory"))?;
    let needed_bits = count
        .checked_mul(usize::from(field_bits))
        .ok_or_else(|| invalid_media("stz2 sample count overflows"))?;
    if !matches!(field_bits, 4 | 8 | 16) {
        return Err(invalid_media("stz2 field size must be 4, 8, or 16 bits"));
    }
    if needed_bits.div_ceil(8) > reader.remaining() {
        return Err(invalid_media("stz2 entry count exceeds its box"));
    }
    let mut sizes = Vec::with_capacity(count);
    match field_bits {
        4 => {
            // Two entries per byte, the first in the high nibble.
            while sizes.len() < count {
                let byte = reader.u8()?;
                sizes.push(u32::from(byte >> 4));
                if sizes.len() < count {
                    sizes.push(u32::from(byte & 0x0f));
                }
            }
        }
        8 => {
            for _ in 0..count {
                sizes.push(u32::from(reader.u8()?));
            }
        }
        _ => {
            for _ in 0..count {
                sizes.push(u32::from(reader.u16()?));
            }
        }
    }
    Ok(SampleSizes::Table(sizes))
}

fn parse_chunk_offsets(payload: &[u8], wide: bool) -> Result<Vec<u64>> {
    let mut reader = Reader::new(payload);
    reader.full_box()?;
    let count = reader.entry_count(if wide { 8 } else { 4 })?;
    (0..count)
        .map(|_| {
            if wide {
                reader.u64()
            } else {
                reader.u32().map(u64::from)
            }
        })
        .collect()
}

/// Expands the tables into one record per sample, checking every byte range against the source.
pub(super) fn expand_samples(
    tables: &SampleTables,
    source_len: u64,
    limits: &LimitsConfig,
) -> Result<Vec<Sample>> {
    let sample_count = tables.sizes.count();
    if sample_count > limits.max_samples_per_track {
        return Err(invalid_media("sample count exceeds configured limit"));
    }
    let sizes = sample_sizes(&tables.sizes, sample_count);
    let byte_offsets = sample_offsets(tables, &sizes, sample_count)?;
    let times = sample_times(&tables.time_to_sample, sample_count)?;
    let composition_offsets = composition_offsets(tables.composition.as_deref(), sample_count)?;
    let sync_samples = tables
        .sync
        .as_ref()
        .map(|entries| entries.iter().copied().collect::<HashSet<_>>());

    let mut samples = Vec::with_capacity(sample_count);
    for index in 0..sample_count {
        let sample_number = u32::try_from(index)
            .ok()
            .and_then(|number| number.checked_add(1))
            .ok_or_else(|| invalid_media("sample number overflow"))?;
        let offset = byte_offsets[index];
        let size = sizes[index];
        let end = offset
            .checked_add(u64::from(size))
            .ok_or_else(|| invalid_media("sample byte range overflow"))?;
        if end > source_len {
            return Err(invalid_media("sample byte range exceeds source length"));
        }
        samples.push(Sample {
            offset,
            size,
            decode_time: times[index].0,
            duration: times[index].1,
            composition_offset: composition_offsets[index],
            is_sync: sync_samples
                .as_ref()
                .is_none_or(|numbers| numbers.contains(&sample_number)),
        });
    }
    Ok(samples)
}

fn sample_sizes(sizes: &SampleSizes, sample_count: usize) -> Vec<u32> {
    match sizes {
        SampleSizes::Constant { size, .. } => vec![*size; sample_count],
        SampleSizes::Table(sizes) => sizes.clone(),
    }
}

fn sample_offsets(tables: &SampleTables, sizes: &[u32], sample_count: usize) -> Result<Vec<u64>> {
    let chunks = &tables.chunk_offsets;
    if tables.sample_to_chunk.is_empty() {
        return Err(invalid_media("missing stsc entries"));
    }
    let mut offsets = Vec::with_capacity(sample_count);
    let mut sample_index = 0usize;
    for (entry_index, (first_chunk, samples_per_chunk)) in tables.sample_to_chunk.iter().enumerate()
    {
        if *first_chunk == 0 || *samples_per_chunk == 0 {
            return Err(invalid_media("invalid stsc entry"));
        }
        let next_first_chunk = tables
            .sample_to_chunk
            .get(entry_index + 1)
            .map_or(chunks.len() as u64 + 1, |next| u64::from(next.0));

        for chunk_number in u64::from(*first_chunk)..next_first_chunk {
            let chunk_index = usize::try_from(chunk_number - 1)
                .map_err(|_| invalid_media("chunk index does not fit in memory"))?;
            let mut offset = *chunks
                .get(chunk_index)
                .ok_or_else(|| invalid_media("stsc references a missing chunk"))?;
            for _ in 0..*samples_per_chunk {
                if sample_index == sample_count {
                    return Err(invalid_media("stsc maps more samples than stsz"));
                }
                offsets.push(offset);
                offset = offset
                    .checked_add(u64::from(sizes[sample_index]))
                    .ok_or_else(|| invalid_media("sample offset overflow"))?;
                sample_index += 1;
            }
        }
    }
    if sample_index != sample_count {
        return Err(invalid_media("stsc maps fewer samples than stsz"));
    }
    Ok(offsets)
}

fn sample_times(runs: &[(u32, u32)], sample_count: usize) -> Result<Vec<(u64, u32)>> {
    let mut times = Vec::with_capacity(sample_count);
    let mut decode_time = 0u64;
    for (run_length, delta) in runs {
        // Bound the running total before expanding: a single run-length entry can claim
        // billions of samples, and expansion must never outgrow the already-limited count.
        let run = usize::try_from(*run_length)
            .ok()
            .filter(|run| times.len().saturating_add(*run) <= sample_count)
            .ok_or_else(|| invalid_media("stts entry count does not match sample count"))?;
        for _ in 0..run {
            times.push((decode_time, *delta));
            decode_time = decode_time
                .checked_add(u64::from(*delta))
                .ok_or_else(|| invalid_media("decode timestamp overflow"))?;
        }
    }
    if times.len() != sample_count {
        return Err(invalid_media(
            "stts entry count does not match sample count",
        ));
    }
    Ok(times)
}

fn composition_offsets(runs: Option<&[(u32, i32)]>, sample_count: usize) -> Result<Vec<i32>> {
    let Some(runs) = runs else {
        return Ok(vec![0; sample_count]);
    };
    let mut offsets = Vec::with_capacity(sample_count);
    for (run_length, offset) in runs {
        let run = usize::try_from(*run_length)
            .ok()
            .filter(|run| offsets.len().saturating_add(*run) <= sample_count)
            .ok_or_else(|| invalid_media("ctts entry count does not match sample count"))?;
        offsets.extend(std::iter::repeat_n(*offset, run));
    }
    if offsets.len() != sample_count {
        return Err(invalid_media(
            "ctts entry count does not match sample count",
        ));
    }
    Ok(offsets)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full box payload: version, zero flags, then `body`.
    fn full(version: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![version, 0, 0, 0];
        bytes.extend_from_slice(body);
        bytes
    }

    fn be(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect()
    }

    fn table(sizes: Vec<u32>) -> SampleTables {
        SampleTables {
            time_to_sample: vec![(u32::try_from(sizes.len()).unwrap(), 10)],
            composition: None,
            sync: None,
            sample_to_chunk: vec![(1, u32::try_from(sizes.len()).unwrap())],
            sizes: SampleSizes::Table(sizes),
            chunk_offsets: vec![100],
        }
    }

    #[test]
    fn stz2_reads_four_bit_sizes_high_nibble_first() {
        // Five entries in three bytes: 1 2 | 3 4 | 5 (padding).
        let payload = full(0, &[0, 0, 0, 4, 0, 0, 0, 5, 0x12, 0x34, 0x50]);

        let SampleSizes::Table(sizes) = parse_stz2(&payload).unwrap() else {
            panic!("stz2 always yields a table");
        };

        assert_eq!(sizes, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn stz2_reads_eight_and_sixteen_bit_sizes() {
        let eight = full(0, &[0, 0, 0, 8, 0, 0, 0, 2, 7, 200]);
        let sixteen = full(0, &[0, 0, 0, 16, 0, 0, 0, 2, 0x01, 0x00, 0xff, 0xff]);

        let SampleSizes::Table(eight) = parse_stz2(&eight).unwrap() else {
            panic!("table");
        };
        let SampleSizes::Table(sixteen) = parse_stz2(&sixteen).unwrap() else {
            panic!("table");
        };

        assert_eq!(eight, [7, 200]);
        assert_eq!(sixteen, [256, 65535]);
    }

    #[test]
    fn stz2_rejects_bad_field_sizes_and_counts_that_outrun_the_box() {
        let bad_width = full(0, &[0, 0, 0, 12, 0, 0, 0, 1, 0, 0]);
        let too_many = full(0, &[0, 0, 0, 8, 0xff, 0xff, 0xff, 0xff, 1, 2]);

        assert!(parse_stz2(&bad_width).is_err());
        assert!(parse_stz2(&too_many).is_err());
    }

    #[test]
    fn stsz_with_a_constant_size_has_no_table() {
        let payload = full(0, &be(&[1024, 5000]));

        let SampleSizes::Constant { size, count } = parse_stsz(&payload).unwrap() else {
            panic!("constant");
        };

        assert_eq!((size, count), (1024, 5000));
    }

    #[test]
    fn stsz_table_cannot_claim_more_entries_than_the_box_holds() {
        let payload = full(0, &be(&[0, 1_000_000, 1, 2, 3]));

        assert!(parse_stsz(&payload).is_err());
    }

    #[test]
    fn ctts_version_one_keeps_negative_offsets() {
        let payload = full(
            1,
            &[
                &be(&[2])[..],
                &be(&[3])[..],
                &(-1024i32).to_be_bytes()[..],
                &be(&[1])[..],
                &be(&[2048])[..],
            ]
            .concat(),
        );

        let runs = parse_ctts(&payload).unwrap();

        assert_eq!(runs, [(3, -1024), (1, 2048)]);
    }

    #[test]
    fn ctts_version_zero_rejects_offsets_that_would_read_as_negative() {
        let payload = full(0, &be(&[1, 1, 0x8000_0000]));

        assert!(parse_ctts(&payload).is_err());
    }

    #[test]
    fn a_table_entry_count_that_outruns_its_box_is_rejected() {
        let payload = full(0, &be(&[0x00ff_ffff, 1, 10]));

        assert!(parse_runs(&payload).is_err());
        assert!(parse_stss(&full(0, &be(&[u32::MAX]))).is_err());
        assert!(parse_chunk_offsets(&full(0, &be(&[u32::MAX])), true).is_err());
    }

    #[test]
    fn chunk_offsets_read_in_either_width() {
        let narrow = full(0, &be(&[2, 100, 200]));
        let wide = full(
            0,
            &[&be(&[1])[..], &0x1_0000_0000u64.to_be_bytes()[..]].concat(),
        );

        assert_eq!(parse_chunk_offsets(&narrow, false).unwrap(), [100, 200]);
        assert_eq!(parse_chunk_offsets(&wide, true).unwrap(), [0x1_0000_0000]);
    }

    #[test]
    fn expansion_places_samples_by_chunk_runs() {
        // Chunks at 1000 and 5000; the first chunk holds two samples, later chunks one each.
        let tables = SampleTables {
            time_to_sample: vec![(4, 10)],
            composition: None,
            sync: Some(vec![1, 3]),
            sample_to_chunk: vec![(1, 2), (2, 1)],
            sizes: SampleSizes::Table(vec![10, 20, 30, 40]),
            chunk_offsets: vec![1000, 5000, 6000],
        };

        let samples = expand_samples(&tables, 10_000, &LimitsConfig::default()).unwrap();

        let offsets = samples
            .iter()
            .map(|sample| sample.offset)
            .collect::<Vec<_>>();
        assert_eq!(offsets, [1000, 1010, 5000, 6000]);
        let sync = samples
            .iter()
            .map(|sample| sample.is_sync)
            .collect::<Vec<_>>();
        assert_eq!(sync, [true, false, true, false]);
        assert_eq!(samples[3].decode_time, 30);
    }

    #[test]
    fn a_constant_size_table_cannot_claim_more_samples_than_the_limit_allows() {
        let mut tables = table(vec![]);
        tables.sizes = SampleSizes::Constant {
            size: 1,
            count: u32::MAX,
        };

        // Must fail on the count alone, without trying to allocate four billion samples.
        let error = expand_samples(&tables, u64::MAX, &LimitsConfig::default()).unwrap_err();

        assert!(error.to_string().contains("sample count"), "{error}");
    }

    #[test]
    fn expansion_rejects_samples_beyond_the_source() {
        let tables = table(vec![50, 60]);

        // The chunk starts at 100, so the samples end at 100 + 50 + 60 = 210.
        assert!(expand_samples(&tables, 209, &LimitsConfig::default()).is_err());
        assert!(expand_samples(&tables, 210, &LimitsConfig::default()).is_ok());
    }

    #[test]
    fn expansion_rejects_a_time_run_that_claims_extra_samples() {
        let mut tables = table(vec![1, 1]);
        tables.time_to_sample = vec![(u32::MAX, 10)];

        assert!(expand_samples(&tables, 1000, &LimitsConfig::default()).is_err());
    }
}
