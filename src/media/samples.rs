//! A track's samples, kept as compact tables rather than one record per sample.
//!
//! See `docs/technical-design/0010-compact-sample-index.md`. Only sizes are stored per sample;
//! offsets come from a chunk table, decode times and durations from runs, composition offsets
//! from runs, and sync samples from a sorted list. A [`SampleIndex`] is a window over those shared
//! tables with a decode-time shift, so trimming and moving a track never copies them.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use super::Sample;
use crate::error::{Error, Result};

/// The samples of one track: a window over shared tables, with every decode time moved by
/// `shift`. Cloning is cheap.
#[derive(Clone)]
pub(crate) struct SampleIndex {
    tables: Arc<Tables>,
    start: usize,
    end: usize,
    shift: i128,
}

/// The tables of one track as parsed, shared by every window cut from it.
#[derive(Debug, Default)]
pub(crate) struct Tables {
    pub(crate) sizes: Vec<u32>,
    /// Each chunk's first sample, ascending, starting at 0. Only chunks that hold samples.
    pub(crate) chunk_first: Vec<u32>,
    /// Each chunk's byte offset in the source.
    pub(crate) chunk_offset: Vec<u64>,
    /// Each timing run's first sample, ascending, starting at 0. A run's samples have equal
    /// durations and follow each other without a gap.
    pub(crate) time_first: Vec<u32>,
    /// Each timing run's first decode time.
    pub(crate) time_start: Vec<u64>,
    /// Each timing run's sample duration.
    pub(crate) time_duration: Vec<u32>,
    /// Each composition run's first sample, ascending, starting at 0; empty when every offset is
    /// zero.
    pub(crate) composition_first: Vec<u32>,
    pub(crate) composition_offset: Vec<i32>,
    /// Sync samples, ascending and unique, or `None` when every sample is one.
    pub(crate) sync: Option<Vec<u32>>,
}

/// The run (or chunk) holding sample `index`, given each run's ascending first sample.
fn run_of(first: &[u32], index: usize) -> usize {
    first
        .partition_point(|&first| first as usize <= index)
        .saturating_sub(1)
}

impl Tables {
    fn len(&self) -> usize {
        self.sizes.len()
    }

    fn decode_time(&self, index: usize) -> u64 {
        let run = run_of(&self.time_first, index);
        self.time_start[run]
            + (index - self.time_first[run] as usize) as u64 * u64::from(self.time_duration[run])
    }

    fn duration(&self, index: usize) -> u32 {
        self.time_duration[run_of(&self.time_first, index)]
    }

    fn offset(&self, index: usize) -> u64 {
        let chunk = run_of(&self.chunk_first, index);
        let first = self.chunk_first[chunk] as usize;
        self.chunk_offset[chunk]
            + self.sizes[first..index]
                .iter()
                .map(|&size| u64::from(size))
                .sum::<u64>()
    }

    fn composition_offset(&self, index: usize) -> i32 {
        if self.composition_first.is_empty() {
            0
        } else {
            self.composition_offset[run_of(&self.composition_first, index)]
        }
    }

    fn is_sync(&self, index: usize) -> bool {
        self.sync.as_ref().is_none_or(|sync| {
            u32::try_from(index).is_ok_and(|index| sync.binary_search(&index).is_ok())
        })
    }

    /// The first sample whose decode time is at or after `time`, or `len` if none is.
    fn partition_by_decode_time(&self, time: i128) -> usize {
        let runs = self.time_first.len();
        // The first run starting at or after `time`; anything earlier is in the run before it.
        let next = self
            .time_start
            .partition_point(|&start| i128::from(start) < time);
        let next_first = self
            .time_first
            .get(next)
            .map_or(self.len(), |&first| first as usize);
        let Some(run) = next.checked_sub(1) else {
            return 0;
        };
        let run_end = if next < runs { next_first } else { self.len() };
        let duration = i128::from(self.time_duration[run]);
        if duration == 0 {
            return next_first;
        }
        let elapsed = time - i128::from(self.time_start[run]);
        let steps = usize::try_from((elapsed + duration - 1) / duration).unwrap_or(usize::MAX);
        let candidate = (self.time_first[run] as usize).saturating_add(steps);
        if candidate < run_end {
            candidate
        } else {
            next_first
        }
    }

    fn bytes(&self) -> usize {
        self.sizes.len() * 4
            + self.chunk_first.len() * 12
            + self.time_first.len() * 16
            + self.composition_first.len() * 8
            + self.sync.as_ref().map_or(0, |sync| sync.len() * 4)
    }
}

impl SampleIndex {
    /// Every sample of `tables`, unshifted. The tables must describe a valid track: see
    /// `mp4::tables` and [`Self::from_samples`], which check what they build.
    pub(crate) fn new(tables: Tables) -> Self {
        let end = tables.len();
        Self {
            tables: Arc::new(tables),
            start: 0,
            end,
            shift: 0,
        }
    }

    /// Builds the tables for samples given one by one: fragmented input, and tests. Samples
    /// that follow each other in the file share a chunk, and samples with equal durations that
    /// follow each other in time share a timing run.
    pub(crate) fn from_samples(samples: &[Sample]) -> Self {
        let mut tables = Tables {
            sizes: Vec::with_capacity(samples.len()),
            ..Tables::default()
        };
        let mut sync = Vec::new();
        let mut previous: Option<&Sample> = None;
        for (index, sample) in samples.iter().enumerate() {
            let number = u32::try_from(index).expect("sample counts are limited to u32");
            let same_chunk = previous.is_some_and(|previous| {
                previous.offset + u64::from(previous.size) == sample.offset
            });
            if !same_chunk {
                tables.chunk_first.push(number);
                tables.chunk_offset.push(sample.offset);
            }
            let same_run = previous.is_some_and(|previous| {
                previous.duration == sample.duration
                    && previous.decode_time + u64::from(previous.duration) == sample.decode_time
            });
            if !same_run {
                tables.time_first.push(number);
                tables.time_start.push(sample.decode_time);
                tables.time_duration.push(sample.duration);
            }
            if tables.composition_offset.last() != Some(&sample.composition_offset) {
                tables.composition_first.push(number);
                tables.composition_offset.push(sample.composition_offset);
            }
            if sample.is_sync {
                sync.push(number);
            }
            tables.sizes.push(sample.size);
            previous = Some(sample);
        }
        if tables.composition_offset == [0] {
            tables.composition_first.clear();
            tables.composition_offset.clear();
        }
        tables.sync = (sync.len() != samples.len()).then_some(sync);
        Self::new(tables)
    }

    pub(crate) fn len(&self) -> usize {
        self.end - self.start
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.start == self.end
    }

    fn absolute(&self, index: usize) -> usize {
        assert!(
            index < self.len(),
            "sample {index} is outside a track of {}",
            self.len()
        );
        self.start + index
    }

    fn moved(&self, time: u64) -> u64 {
        // `shifted` checked the first and last decode times, and decode times only grow.
        u64::try_from(i128::from(time) + self.shift).expect("shifted decode times stay in range")
    }

    /// Sample `index`. Each call searches the tables; use [`Self::iter`] or [`Self::range`] to
    /// read samples in order.
    pub(crate) fn get(&self, index: usize) -> Sample {
        let index = self.absolute(index);
        let tables = &self.tables;
        Sample {
            offset: tables.offset(index),
            size: tables.sizes[index],
            decode_time: self.moved(tables.decode_time(index)),
            duration: tables.duration(index),
            composition_offset: tables.composition_offset(index),
            is_sync: tables.is_sync(index),
        }
    }

    pub(crate) fn first(&self) -> Option<Sample> {
        (!self.is_empty()).then(|| self.get(0))
    }

    pub(crate) fn last(&self) -> Option<Sample> {
        self.len().checked_sub(1).map(|index| self.get(index))
    }

    pub(crate) fn size(&self, index: usize) -> u32 {
        self.tables.sizes[self.absolute(index)]
    }

    pub(crate) fn decode_time(&self, index: usize) -> u64 {
        self.moved(self.tables.decode_time(self.absolute(index)))
    }

    /// The first sample whose decode time is at or after `time`, or `len()` if none is; what
    /// `partition_point(|sample| sample.decode_time < time)` gives on a slice.
    pub(crate) fn partition_by_decode_time(&self, time: u64) -> usize {
        let stored = i128::from(time) - self.shift;
        self.tables
            .partition_by_decode_time(stored)
            .clamp(self.start, self.end)
            - self.start
    }

    /// The sync samples, by their index in this window, in order.
    pub(crate) fn sync_indices(&self) -> Box<dyn Iterator<Item = usize> + '_> {
        match &self.tables.sync {
            None => Box::new(0..self.len()),
            Some(sync) => {
                let (low, high) = self.sync_bounds(sync);
                Box::new(
                    sync[low..high]
                        .iter()
                        .map(|&index| index as usize - self.start),
                )
            }
        }
    }

    /// Whether every sample is a sync sample, so there is no sync table to walk.
    pub(crate) fn every_sample_is_sync(&self) -> bool {
        self.tables.sync.is_none()
    }

    /// The `nth` sync sample's index in this window.
    pub(crate) fn nth_sync(&self, nth: usize) -> Option<usize> {
        match &self.tables.sync {
            None => (nth < self.len()).then_some(nth),
            Some(sync) => {
                let (low, high) = self.sync_bounds(sync);
                let position = low.checked_add(nth).filter(|position| *position < high)?;
                Some(sync[position] as usize - self.start)
            }
        }
    }

    fn sync_bounds(&self, sync: &[u32]) -> (usize, usize) {
        (
            sync.partition_point(|&index| (index as usize) < self.start),
            sync.partition_point(|&index| (index as usize) < self.end),
        )
    }

    /// The bytes of the samples in `range`. Cannot overflow: at most `u32::MAX` sizes of at
    /// most `u32::MAX` bytes each.
    pub(crate) fn payload_bytes(&self, range: Range<usize>) -> u64 {
        assert!(
            range.start <= range.end && range.end <= self.len(),
            "range outside the track"
        );
        self.tables.sizes[self.start + range.start..self.start + range.end]
            .iter()
            .map(|&size| u64::from(size))
            .sum()
    }

    /// Every sample, in order.
    pub(crate) fn iter(&self) -> Iter<'_> {
        self.range(0..self.len())
    }

    /// The samples in `range`, in order.
    pub(crate) fn range(&self, range: Range<usize>) -> Iter<'_> {
        assert!(
            range.start <= range.end && range.end <= self.len(),
            "range outside the track"
        );
        let position = self.start + range.start;
        let tables = &*self.tables;
        let in_range = position < tables.len();
        Iter {
            index: self,
            position,
            end: self.start + range.end,
            chunk: if in_range {
                run_of(&tables.chunk_first, position)
            } else {
                0
            },
            offset: if in_range { tables.offset(position) } else { 0 },
            time_run: if in_range {
                run_of(&tables.time_first, position)
            } else {
                0
            },
            composition_run: if in_range && !tables.composition_first.is_empty() {
                run_of(&tables.composition_first, position)
            } else {
                0
            },
            sync: tables.sync.as_ref().map_or(0, |sync| {
                sync.partition_point(|&index| (index as usize) < position)
            }),
        }
    }

    /// The samples in `range`, collected.
    pub(crate) fn to_vec(&self, range: Range<usize>) -> Vec<Sample> {
        self.range(range).collect()
    }

    /// Every sample, collected: for tests that index into a track.
    #[cfg(test)]
    pub(crate) fn all(&self) -> Vec<Sample> {
        self.iter().collect()
    }

    /// The samples in `range` only, sharing these tables.
    pub(crate) fn window(&self, range: Range<usize>) -> Self {
        assert!(
            range.start <= range.end && range.end <= self.len(),
            "range outside the track"
        );
        Self {
            tables: Arc::clone(&self.tables),
            start: self.start + range.start,
            end: self.start + range.end,
            shift: self.shift,
        }
    }

    /// The same samples with every decode time moved by `delta` ticks. Fails, with `message`,
    /// if a decode time or the end of the last sample would leave `0..=u64::MAX`.
    pub(crate) fn shifted(&self, delta: i128, message: &str) -> Result<Self> {
        let moved = Self {
            shift: self.shift + delta,
            ..self.clone()
        };
        if let (Some(first), Some(last)) = (self.first(), self.last()) {
            let first = i128::from(first.decode_time) + delta;
            let end = i128::from(last.decode_time) + i128::from(last.duration) + delta;
            if first < 0 || u64::try_from(end).is_err() {
                return Err(Error::InvalidMedia(message.to_owned()));
            }
        }
        Ok(moved)
    }

    /// Bytes held by the tables behind this window.
    pub(crate) fn table_bytes(&self) -> usize {
        self.tables.bytes()
    }
}

/// Reads samples in order, keeping a cursor into each table so each sample costs a few
/// comparisons instead of a search.
pub(crate) struct Iter<'a> {
    index: &'a SampleIndex,
    position: usize,
    end: usize,
    chunk: usize,
    offset: u64,
    time_run: usize,
    composition_run: usize,
    sync: usize,
}

impl Iterator for Iter<'_> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        if self.position >= self.end {
            return None;
        }
        let position = self.position;
        let tables = &*self.index.tables;
        let at = |first: &[u32], run: usize| {
            first
                .get(run)
                .is_some_and(|&first| first as usize <= position)
        };
        while at(&tables.chunk_first, self.chunk + 1) {
            self.chunk += 1;
            self.offset = tables.chunk_offset[self.chunk];
        }
        while at(&tables.time_first, self.time_run + 1) {
            self.time_run += 1;
        }
        while at(&tables.composition_first, self.composition_run + 1) {
            self.composition_run += 1;
        }
        let size = tables.sizes[position];
        let run = self.time_run;
        let decode_time = tables.time_start[run]
            + (position - tables.time_first[run] as usize) as u64
                * u64::from(tables.time_duration[run]);
        let is_sync = match &tables.sync {
            None => true,
            Some(sync) => {
                let hit = sync
                    .get(self.sync)
                    .is_some_and(|&index| index as usize == position);
                if hit {
                    self.sync += 1;
                }
                hit
            }
        };
        let sample = Sample {
            offset: self.offset,
            size,
            decode_time: self.index.moved(decode_time),
            duration: tables.time_duration[run],
            composition_offset: tables
                .composition_offset
                .get(self.composition_run)
                .copied()
                .unwrap_or_default(),
            is_sync,
        };
        self.offset += u64::from(size);
        self.position += 1;
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.end - self.position;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl PartialEq for SampleIndex {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl Eq for SampleIndex {}

impl fmt::Debug for SampleIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SampleIndex")
            .field("samples", &self.len())
            .field("window", &(self.start..self.end))
            .field("shift", &self.shift)
            .finish_non_exhaustive()
    }
}

impl<'a> IntoIterator for &'a SampleIndex {
    type Item = Sample;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl From<Vec<Sample>> for SampleIndex {
    fn from(samples: Vec<Sample>) -> Self {
        Self::from_samples(&samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(offset: u64, size: u32, decode_time: u64, duration: u32) -> Sample {
        Sample {
            offset,
            size,
            decode_time,
            duration,
            composition_offset: 0,
            is_sync: false,
        }
    }

    /// Two chunks, a duration change, a gap in time, B-frame offsets, and sparse sync samples.
    fn samples() -> Vec<Sample> {
        let mut samples = vec![
            sample(100, 10, 0, 512),
            sample(110, 20, 512, 512),
            sample(130, 30, 1024, 512),
            sample(500, 40, 1536, 1024),
            sample(540, 50, 2560, 1024),
            sample(590, 60, 5000, 1024),
            sample(650, 70, 6024, 1024),
        ];
        for (index, offset) in [(1, 1024), (2, -512), (4, 2048)] {
            samples[index].composition_offset = offset;
        }
        for index in [0, 3, 6] {
            samples[index].is_sync = true;
        }
        samples
    }

    #[test]
    fn lookups_and_iteration_give_back_every_sample() {
        let samples = samples();
        let index = SampleIndex::from_samples(&samples);
        assert_eq!(index.len(), samples.len());
        assert_eq!(index.iter().collect::<Vec<_>>(), samples);
        for (position, expected) in samples.iter().enumerate() {
            assert_eq!(index.get(position), *expected, "{position}");
            assert_eq!(index.decode_time(position), expected.decode_time);
            for end in position..=samples.len() {
                assert_eq!(index.to_vec(position..end), samples[position..end]);
            }
        }
    }

    #[test]
    fn partitioning_by_decode_time_matches_a_slice() {
        let samples = samples();
        let index = SampleIndex::from_samples(&samples);
        for time in 0..7_500 {
            assert_eq!(
                index.partition_by_decode_time(time),
                samples.partition_point(|sample| sample.decode_time < time),
                "{time}"
            );
        }
    }

    #[test]
    fn windows_and_shifts_share_the_tables() {
        let samples = samples();
        let index = SampleIndex::from_samples(&samples);
        let window = index.window(2..6).shifted(-1000, "overflow").unwrap();
        let expected = samples[2..6]
            .iter()
            .map(|sample| Sample {
                decode_time: sample.decode_time - 1000,
                ..*sample
            })
            .collect::<Vec<_>>();
        assert_eq!(window.iter().collect::<Vec<_>>(), expected);
        assert_eq!(window.get(3), expected[3]);
        assert_eq!(window.sync_indices().collect::<Vec<_>>(), [1]);
        assert_eq!(window.nth_sync(0), Some(1));
        assert_eq!(window.nth_sync(1), None);
        assert_eq!(window.payload_bytes(0..4), 30 + 40 + 50 + 60);
        for time in 0..7_000 {
            assert_eq!(
                window.partition_by_decode_time(time),
                expected.partition_point(|sample| sample.decode_time < time),
                "{time}"
            );
        }
        // Only the window's own first sample counts: 24 ticks before it is fine, 25 is not.
        assert!(window.shifted(-24, "overflow").is_ok());
        assert!(window.shifted(-25, "overflow").is_err());
        assert!(index.shifted(i128::from(u64::MAX), "overflow").is_err());
    }

    #[test]
    fn a_track_of_only_sync_samples_needs_no_sync_table() {
        let samples = (0..5)
            .map(|index| Sample {
                is_sync: true,
                ..sample(index * 10, 10, index * 1024, 1024)
            })
            .collect::<Vec<_>>();
        let index = SampleIndex::from_samples(&samples);
        assert!(index.tables.sync.is_none());
        assert_eq!(index.tables.chunk_first, [0]);
        assert_eq!(index.tables.time_first, [0]);
        assert_eq!(index.tables.composition_first, Vec::<u32>::new());
        assert_eq!(index.sync_indices().count(), 5);
        assert_eq!(index.window(1..3).nth_sync(1), Some(1));
    }

    #[test]
    fn equal_indexes_compare_by_their_samples() {
        let samples = samples();
        let whole = SampleIndex::from_samples(&samples);
        assert_eq!(
            whole.window(1..4),
            SampleIndex::from_samples(&samples[1..4])
        );
        assert_ne!(whole.window(1..4), whole.window(1..5));
    }
}
