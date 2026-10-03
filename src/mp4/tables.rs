//! Sample tables (`stbl`): parsing the boxes, then expanding them into per-sample records.
//!
//! Tables are run-length encoded, so a few bytes can describe billions of samples. Every count
//! is checked against the box that holds it before anything is allocated, and expansion is
//! checked against the configured sample limit before it begins.

use super::boxes::{Reader, be_u32s, invalid_media, optional_child, required_child};
use crate::config::LimitsConfig;
use crate::error::Result;
#[cfg(test)]
use crate::media::Sample;
use crate::media::{SampleIndex, SampleTablesData as Tables};

/// The raw sample tables of one track.
#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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
    Ok(pairs(reader.entries(8)?).collect())
}

/// `ctts` layout: `(count, offset)` pairs; version 0 offsets are unsigned, version 1 signed.
fn parse_ctts(payload: &[u8]) -> Result<Vec<(u32, i32)>> {
    let mut reader = Reader::new(payload);
    let version = reader.full_box()?;
    pairs(reader.entries(8)?)
        .map(|(run, offset)| {
            let offset = if version == 0 {
                i32::try_from(offset)
                    .map_err(|_| invalid_media("ctts version 0 offset is out of range"))?
            } else {
                offset.cast_signed()
            };
            Ok((run, offset))
        })
        .collect()
}

fn parse_stss(payload: &[u8]) -> Result<Vec<u32>> {
    let mut reader = Reader::new(payload);
    reader.full_box()?;
    Ok(be_u32s(reader.entries(4)?).collect())
}

/// `stsc` layout: `(first_chunk, samples_per_chunk, sample_description_index)` triples.
fn parse_stsc(payload: &[u8]) -> Result<Vec<(u32, u32)>> {
    let mut reader = Reader::new(payload);
    reader.full_box()?;
    Ok(reader
        .entries(12)?
        .as_chunks::<12>()
        .0
        .iter()
        .map(|entry| (be_u32(&entry[0..4]), be_u32(&entry[4..8])))
        .collect())
}

/// Entries of two big-endian `u32`s each.
fn pairs(bytes: &[u8]) -> impl Iterator<Item = (u32, u32)> + '_ {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|entry| (be_u32(&entry[0..4]), be_u32(&entry[4..8])))
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("four bytes"))
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
        be_u32s(reader.take(entries * 4)?).collect(),
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
    if wide {
        Ok(reader
            .entries(8)?
            .as_chunks::<8>()
            .0
            .iter()
            .map(|entry| u64::from_be_bytes(*entry))
            .collect())
    } else {
        Ok(be_u32s(reader.entries(4)?).map(u64::from).collect())
    }
}

/// Builds the compact index of the tables (TDD 0010), checking what expanding them would: the
/// configured sample limit, that `stsc`, `stts`, and `ctts` each account for exactly the samples
/// `stsz` has, and that every sample's bytes lie inside the source. Checks fail in the same
/// order, with the same messages, as the one-record-per-sample expansion they replace.
pub(super) fn sample_index(
    tables: SampleTables,
    source_len: u64,
    limits: &LimitsConfig,
) -> Result<SampleIndex> {
    let sample_count = tables.sizes.count();
    if sample_count > limits.max_samples_per_track {
        return Err(invalid_media("sample count exceeds configured limit"));
    }
    let SampleTables {
        time_to_sample,
        composition,
        sync,
        sample_to_chunk,
        sizes,
        chunk_offsets,
    } = tables;
    let sizes = match sizes {
        SampleSizes::Table(sizes) => sizes,
        SampleSizes::Constant { size, .. } => vec![size; sample_count],
    };
    let chunks = chunks(&sample_to_chunk, chunk_offsets, &sizes, source_len)?;
    let timing = timing_runs(&time_to_sample, sample_count)?;
    let (composition_first, composition_offset) = match composition.as_deref() {
        Some(runs) => composition_runs(runs, sample_count)?,
        None => (Vec::new(), Vec::new()),
    };
    if let Some(message) = chunks.beyond_source {
        return Err(invalid_media(message));
    }
    // `stss` lists one-based sample numbers, in no required order; numbers outside the track
    // are ignored.
    let sync = sync.map(|numbers| {
        let mut sync = numbers
            .iter()
            .filter_map(|&number| number.checked_sub(1))
            .filter(|&index| (index as usize) < sample_count)
            .collect::<Vec<_>>();
        sync.sort_unstable();
        sync.dedup();
        sync
    });
    Ok(SampleIndex::new(Tables {
        sizes,
        chunk_first: chunks.first,
        chunk_offset: chunks.offset,
        time_first: timing.first,
        time_start: timing.start,
        time_duration: timing.duration,
        composition_first,
        composition_offset,
        sync,
    }))
}

/// The chunk table: each chunk's first sample and byte offset.
struct Chunks {
    first: Vec<u32>,
    offset: Vec<u64>,
    /// The first byte-range failure, which is reported only after the timing tables are
    /// checked, where the one-record-per-sample expansion found it.
    beyond_source: Option<&'static str>,
}

/// Each chunk's first sample and offset, from `stsc` and `stco`/`co64`.
///
/// When `stsc` uses every chunk once, in order, which is how files are written, `offsets` is
/// kept as the chunk table rather than copied.
fn chunks(
    sample_to_chunk: &[(u32, u32)],
    mut offsets: Vec<u64>,
    sizes: &[u32],
    source_len: u64,
) -> Result<Chunks> {
    let sample_count = sizes.len();
    if sample_to_chunk.is_empty() {
        return Err(invalid_media("missing stsc entries"));
    }
    let mut chunk_first = Vec::with_capacity(offsets.len());
    // `None` while every chunk used so far is the next one in `offsets`.
    let mut chunk_offset: Option<Vec<u64>> = None;
    let mut beyond_source = None;
    let mut mapped = 0usize;
    for (entry_index, &(first_chunk, samples_per_chunk)) in sample_to_chunk.iter().enumerate() {
        if first_chunk == 0 || samples_per_chunk == 0 {
            return Err(invalid_media("invalid stsc entry"));
        }
        let next_first_chunk = sample_to_chunk
            .get(entry_index + 1)
            .map_or(offsets.len() as u64 + 1, |next| u64::from(next.0));
        let per_chunk = samples_per_chunk as usize;
        for chunk_number in u64::from(first_chunk)..next_first_chunk {
            let chunk_index = usize::try_from(chunk_number - 1)
                .map_err(|_| invalid_media("chunk index does not fit in memory"))?;
            let offset = *offsets
                .get(chunk_index)
                .ok_or_else(|| invalid_media("stsc references a missing chunk"))?;
            let take = per_chunk.min(sample_count - mapped);
            // The chunk's samples follow each other, so the last one ends at the offset plus
            // all of their sizes; that is what every per-sample check came down to.
            let end = sizes[mapped..mapped + take]
                .iter()
                .try_fold(offset, |end, &size| end.checked_add(u64::from(size)))
                .ok_or_else(|| invalid_media("sample offset overflow"))?;
            if take > 0 {
                match &mut chunk_offset {
                    None if chunk_index == chunk_first.len() => {}
                    None => {
                        let mut copied = offsets[..chunk_first.len()].to_vec();
                        copied.push(offset);
                        chunk_offset = Some(copied);
                    }
                    Some(copied) => copied.push(offset),
                }
                chunk_first.push(u32::try_from(mapped).expect("sample counts fit in u32"));
                if end > source_len && beyond_source.is_none() {
                    beyond_source = Some("sample byte range exceeds source length");
                }
            }
            mapped += take;
            if take < per_chunk {
                return Err(invalid_media("stsc maps more samples than stsz"));
            }
        }
    }
    if mapped != sample_count {
        return Err(invalid_media("stsc maps fewer samples than stsz"));
    }
    let chunk_offset = chunk_offset.unwrap_or_else(|| {
        offsets.truncate(chunk_first.len());
        offsets
    });
    Ok(Chunks {
        first: chunk_first,
        offset: chunk_offset,
        beyond_source,
    })
}

/// Timing runs: each run's first sample, first decode time, and sample duration.
struct TimingRuns {
    first: Vec<u32>,
    start: Vec<u64>,
    duration: Vec<u32>,
}

/// Timing runs from `stts`.
fn timing_runs(runs: &[(u32, u32)], sample_count: usize) -> Result<TimingRuns> {
    let mismatch = || invalid_media("stts entry count does not match sample count");
    let mut first = Vec::with_capacity(runs.len());
    let mut start = Vec::with_capacity(runs.len());
    let mut duration = Vec::with_capacity(runs.len());
    let mut decode_time = 0u64;
    let mut at = 0usize;
    for &(run_length, delta) in runs {
        let run = usize::try_from(run_length)
            .ok()
            .filter(|run| at.saturating_add(*run) <= sample_count)
            .ok_or_else(mismatch)?;
        if run == 0 {
            continue;
        }
        first.push(u32::try_from(at).expect("sample counts fit in u32"));
        start.push(decode_time);
        duration.push(delta);
        decode_time = (run as u64)
            .checked_mul(u64::from(delta))
            .and_then(|span| decode_time.checked_add(span))
            .ok_or_else(|| invalid_media("decode timestamp overflow"))?;
        at += run;
    }
    if at != sample_count {
        return Err(mismatch());
    }
    Ok(TimingRuns {
        first,
        start,
        duration,
    })
}

/// Composition runs from `ctts`: each run's first sample and offset.
fn composition_runs(runs: &[(u32, i32)], sample_count: usize) -> Result<(Vec<u32>, Vec<i32>)> {
    let mismatch = || invalid_media("ctts entry count does not match sample count");
    let mut first = Vec::with_capacity(runs.len());
    let mut offsets = Vec::with_capacity(runs.len());
    let mut at = 0usize;
    for &(run_length, offset) in runs {
        let run = usize::try_from(run_length)
            .ok()
            .filter(|run| at.saturating_add(*run) <= sample_count)
            .ok_or_else(mismatch)?;
        if run == 0 {
            continue;
        }
        first.push(u32::try_from(at).expect("sample counts fit in u32"));
        offsets.push(offset);
        at += run;
    }
    if at != sample_count {
        return Err(mismatch());
    }
    Ok((first, offsets))
}

/// The expansion `sample_index` replaced, one record per sample: kept as the reference the
/// compact index is tested against (TDD 0010).
#[cfg(test)]
pub(super) fn expand_samples(
    tables: &SampleTables,
    source_len: u64,
    limits: &LimitsConfig,
) -> Result<Vec<Sample>> {
    let sample_count = tables.sizes.count();
    if sample_count > limits.max_samples_per_track {
        return Err(invalid_media("sample count exceeds configured limit"));
    }
    // Each table is applied in its own pass over the one output vector, rather than expanded
    // into an array of its own and zipped at the end. A long file has hundreds of thousands of
    // samples, so the intermediate arrays were megabytes of memory touched and copied on the
    // first viewer's critical path. The checks, and the order they fail in, are unchanged.
    let mut samples = sample_positions(tables, sample_count)?;
    apply_times(&mut samples, &tables.time_to_sample)?;
    if let Some(runs) = tables.composition.as_deref() {
        apply_composition(&mut samples, runs)?;
    }
    if let Some(entries) = tables.sync.as_deref() {
        for sample in &mut samples {
            sample.is_sync = false;
        }
        // Sample numbers outside the track are ignored, as before.
        for &number in entries {
            if let Some(sample) = usize::try_from(number)
                .ok()
                .and_then(|number| number.checked_sub(1))
                .and_then(|index| samples.get_mut(index))
            {
                sample.is_sync = true;
            }
        }
    }
    for sample in &samples {
        let end = sample
            .offset
            .checked_add(u64::from(sample.size))
            .ok_or_else(|| invalid_media("sample byte range overflow"))?;
        if end > source_len {
            return Err(invalid_media("sample byte range exceeds source length"));
        }
    }
    Ok(samples)
}

/// Every sample's size and byte offset, from `stsz` and the chunk tables. Timing fields are
/// zero and every sample is a sync sample until the later passes fill them in.
#[cfg(test)]
fn sample_positions(tables: &SampleTables, sample_count: usize) -> Result<Vec<Sample>> {
    let chunks = &tables.chunk_offsets;
    if tables.sample_to_chunk.is_empty() {
        return Err(invalid_media("missing stsc entries"));
    }
    let size_of = |index: usize| match &tables.sizes {
        SampleSizes::Constant { size, .. } => *size,
        SampleSizes::Table(sizes) => sizes[index],
    };
    let mut samples = Vec::with_capacity(sample_count);
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
                if samples.len() == sample_count {
                    return Err(invalid_media("stsc maps more samples than stsz"));
                }
                let size = size_of(samples.len());
                samples.push(Sample {
                    offset,
                    size,
                    decode_time: 0,
                    duration: 0,
                    composition_offset: 0,
                    is_sync: true,
                });
                offset = offset
                    .checked_add(u64::from(size))
                    .ok_or_else(|| invalid_media("sample offset overflow"))?;
            }
        }
    }
    if samples.len() != sample_count {
        return Err(invalid_media("stsc maps fewer samples than stsz"));
    }
    Ok(samples)
}

/// Decode times and durations from `stts` runs.
#[cfg(test)]
fn apply_times(samples: &mut [Sample], runs: &[(u32, u32)]) -> Result<()> {
    let mismatch = || invalid_media("stts entry count does not match sample count");
    let mut decode_time = 0u64;
    let mut at = 0usize;
    for (run_length, delta) in runs {
        // Bound the running total before expanding: a single run-length entry can claim
        // billions of samples, and expansion must never outgrow the already-limited count.
        let run = usize::try_from(*run_length)
            .ok()
            .filter(|run| at.saturating_add(*run) <= samples.len())
            .ok_or_else(mismatch)?;
        for sample in &mut samples[at..at + run] {
            sample.decode_time = decode_time;
            sample.duration = *delta;
            decode_time = decode_time
                .checked_add(u64::from(*delta))
                .ok_or_else(|| invalid_media("decode timestamp overflow"))?;
        }
        at += run;
    }
    if at != samples.len() {
        return Err(mismatch());
    }
    Ok(())
}

/// Composition offsets from `ctts` runs.
#[cfg(test)]
fn apply_composition(samples: &mut [Sample], runs: &[(u32, i32)]) -> Result<()> {
    let mismatch = || invalid_media("ctts entry count does not match sample count");
    let mut at = 0usize;
    for (run_length, offset) in runs {
        let run = usize::try_from(*run_length)
            .ok()
            .filter(|run| at.saturating_add(*run) <= samples.len())
            .ok_or_else(mismatch)?;
        for sample in &mut samples[at..at + run] {
            sample.composition_offset = *offset;
        }
        at += run;
    }
    if at != samples.len() {
        return Err(mismatch());
    }
    Ok(())
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

    /// A small deterministic generator, so a failure names a seed that reproduces it.
    struct Random(u64);

    impl Random {
        fn below(&mut self, bound: u32) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            u32::try_from(self.0 % u64::from(bound.max(1))).unwrap()
        }
    }

    /// Splits `count` samples into runs of 1 to `longest`.
    fn runs(random: &mut Random, count: u32, longest: u32) -> Vec<u32> {
        let mut runs = Vec::new();
        let mut left = count;
        while left > 0 {
            let run = 1 + random.below(left.min(longest));
            runs.push(run);
            left -= run;
        }
        runs
    }

    /// Consistent tables for a random track, then, half the time, one field broken.
    fn random_tables(random: &mut Random) -> (SampleTables, u64) {
        let count = random.below(40);
        let sizes = (0..count).map(|_| random.below(50)).collect::<Vec<_>>();
        let mut time_to_sample = runs(random, count, 10)
            .into_iter()
            .map(|run| (run, random.below(3) * 512))
            .collect::<Vec<_>>();
        let mut composition = (random.below(2) == 0).then(|| {
            runs(random, count, 4)
                .into_iter()
                .map(|run| (run, i32::try_from(random.below(2048)).unwrap() - 1024))
                .collect::<Vec<_>>()
        });
        // Chunks of 1 to 5 samples, written to `stsc` as runs of equal chunk sizes.
        let per_chunk = runs(random, count, 5);
        let mut sample_to_chunk: Vec<(u32, u32)> = Vec::new();
        for (index, &samples) in per_chunk.iter().enumerate() {
            if sample_to_chunk.last().is_none_or(|last| last.1 != samples) {
                sample_to_chunk.push((u32::try_from(index).unwrap() + 1, samples));
            }
        }
        if sample_to_chunk.is_empty() {
            sample_to_chunk.push((1, 1 + random.below(3)));
        }
        let mut offset = 0u64;
        let mut first = 0usize;
        let mut chunk_offsets = Vec::new();
        for &samples in &per_chunk {
            offset += u64::from(random.below(100));
            chunk_offsets.push(offset);
            let samples = samples as usize;
            offset += sizes[first..first + samples]
                .iter()
                .map(|&size| u64::from(size))
                .sum::<u64>();
            first += samples;
        }
        let mut source_len = offset + u64::from(random.below(3));
        let sync = (random.below(2) == 0).then(|| {
            (0..random.below(8))
                .map(|_| random.below(count + 3))
                .collect()
        });
        let mut sizes = if random.below(8) == 0 {
            SampleSizes::Constant {
                size: sizes.first().copied().unwrap_or(1),
                count,
            }
        } else {
            SampleSizes::Table(sizes)
        };
        if random.below(2) == 0 {
            let nudge = |random: &mut Random, value: &mut u32| {
                *value = match random.below(3) {
                    0 => value.saturating_add(1),
                    1 => value.saturating_sub(1),
                    _ => 0,
                };
            };
            match random.below(9) {
                0 => {
                    if let Some(run) = time_to_sample.last_mut() {
                        nudge(random, &mut run.0);
                    }
                }
                1 => time_to_sample.push((1 + random.below(3), 7)),
                2 => {
                    if let Some(run) = composition.as_mut().and_then(|runs| runs.last_mut()) {
                        nudge(random, &mut run.0);
                    }
                }
                3 => {
                    let entry = random.below(u32::try_from(sample_to_chunk.len()).unwrap());
                    nudge(random, &mut sample_to_chunk[entry as usize].0);
                }
                4 => {
                    let entry = random.below(u32::try_from(sample_to_chunk.len()).unwrap());
                    nudge(random, &mut sample_to_chunk[entry as usize].1);
                }
                5 => {
                    chunk_offsets.pop();
                }
                6 => chunk_offsets.push(offset + 10),
                7 => source_len = source_len.saturating_sub(1 + u64::from(random.below(60))),
                _ => {
                    if let SampleSizes::Table(sizes) = &mut sizes {
                        sizes.pop();
                    }
                }
            }
        }
        let tables = SampleTables {
            time_to_sample,
            composition,
            sync,
            sample_to_chunk,
            sizes,
            chunk_offsets,
        };
        (tables, source_len)
    }

    /// TDD 0010: on valid and broken tables alike, the compact index and the expansion it
    /// replaced agree: the same samples, or the same error.
    #[test]
    fn the_compact_index_agrees_with_the_expansion_on_random_tables() {
        let limits = LimitsConfig::default();
        let mut outcomes = [0usize; 2];
        for seed in 1..20_000u64 {
            let mut random = Random(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let (tables, source_len) = random_tables(&mut random);
            let expected = expand_samples(&tables, source_len, &limits);
            let actual = sample_index(tables.clone(), source_len, &limits).map(|index| index.all());
            match (&expected, &actual) {
                (Ok(expected), Ok(actual)) => {
                    assert_eq!(actual, expected, "seed {seed}: {tables:?}");
                    outcomes[0] += 1;
                }
                (Err(expected), Err(actual)) => {
                    assert_eq!(actual.to_string(), expected.to_string(), "seed {seed}");
                    outcomes[1] += 1;
                }
                _ => panic!("seed {seed}: expected {expected:?}, got {actual:?}, {tables:?}"),
            }
        }
        // Both paths are exercised often enough to mean something.
        assert!(outcomes.iter().all(|&count| count > 2_000), "{outcomes:?}");
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
