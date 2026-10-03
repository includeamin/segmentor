//! The async task that streams one media segment response body.
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::time::{Duration, timeout};

use super::range::ByteInterval;
use super::state::{PermitFailure, acquire_segment_permit};
use crate::asset::PackagedAsset;
use crate::fmp4;
use crate::observability::metrics::{Metrics, StreamAbort};
use crate::source::ByteRange;

/// Streams one segment response as an async task.
///
/// For a clear segment, a job slot is held only while a source read is in flight, never while
/// waiting for the client, so slow readers cannot exhaust `max_segment_jobs`. An encrypted
/// segment is already in memory: its slot arrives as `first_permit` and, with no source read to
/// consume it, is held until the stream ends, which bounds resident encrypted bytes. Each send is
/// bounded by the idle timeout, so a client that stops reading is dropped instead of pinning the
/// task.
pub(crate) struct StreamJob {
    pub(crate) asset: Arc<PackagedAsset>,
    pub(crate) prepared: fmp4::PreparedSegment,
    pub(crate) interval: ByteInterval,
    pub(crate) first_permit: Option<OwnedSemaphorePermit>,
    pub(crate) segment_jobs: Arc<Semaphore>,
    pub(crate) queue_timeout: Duration,
    pub(crate) idle_timeout: Duration,
    pub(crate) chunk_size: usize,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) sender: mpsc::Sender<std::io::Result<Bytes>>,
}

impl StreamJob {
    pub(crate) async fn run(mut self) {
        if let Err(reason) = self.stream().await {
            self.metrics.stream_aborted(reason);
        }
    }

    async fn stream(&mut self) -> Result<(), StreamAbort> {
        let header_end = self.prepared.header.len() as u64;
        if let Some(overlap) = self.interval.overlap(0, header_end) {
            let (Ok(start), Ok(end)) =
                (usize::try_from(overlap.start), usize::try_from(overlap.end))
            else {
                return self.fail("header range does not fit in memory").await;
            };
            // Sent in `chunk_size` pieces like the source ranges, so a header-only (encrypted)
            // segment is also bounded by the idle timeout instead of sitting whole in the queue.
            let chunk_size = self.chunk_size.max(1);
            let mut at = start;
            while at < end {
                let next = end.min(at.saturating_add(chunk_size));
                let chunk = self.prepared.header.slice(at..next);
                self.send(Ok(chunk)).await?;
                at = next;
            }
        }
        let pieces = self.requested_pieces(header_end);
        if let Some(windows) = interleaved_windows(&pieces, self.chunk_size as u64) {
            return self.stream_interleaved(windows).await;
        }
        for group in coalesce(&pieces, self.chunk_size as u64) {
            match group {
                Read::Split(piece) => {
                    // One piece larger than a chunk: read and send it a chunk at a time.
                    let (mut offset, mut remaining) = (piece.offset, piece.length);
                    while remaining != 0 {
                        let length = remaining.min(self.chunk_size as u64);
                        let chunk = self.read(offset, length).await?;
                        self.send(Ok(chunk)).await?;
                        offset += length;
                        remaining -= length;
                    }
                }
                Read::Span { start, end, pieces } => {
                    // Several nearby pieces from one read, copied into one chunk in response
                    // order. Their total is at most the span, which is at most `chunk_size`.
                    let span = self.read(start, end - start).await?;
                    let chunk = if pieces.len() == 1 && pieces[0].offset == start {
                        span.slice(..usize::try_from(pieces[0].length).unwrap_or(usize::MAX))
                    } else {
                        let total = pieces.iter().map(|piece| piece.length).sum::<u64>();
                        let mut joined =
                            Vec::with_capacity(usize::try_from(total).unwrap_or(usize::MAX));
                        for piece in pieces {
                            let from = usize::try_from(piece.offset - start).unwrap_or(usize::MAX);
                            let to = usize::try_from(piece.offset - start + piece.length)
                                .unwrap_or(usize::MAX);
                            let Some(bytes) = span.get(from..to) else {
                                return self.fail("source read was shorter than requested").await;
                            };
                            joined.extend_from_slice(bytes);
                        }
                        Bytes::from(joined)
                    };
                    self.send(Ok(chunk)).await?;
                }
            }
        }
        Ok(())
    }

    /// Streams a segment whose tracks are interleaved in the file (a muxed one) with each file
    /// region read once. The first track's pieces go out as each window arrives; the others are
    /// held, which `interleaved_windows` bounds, and follow once the first is done, so the
    /// response bytes are exactly those of reading each track's ranges in turn.
    async fn stream_interleaved(&mut self, windows: Vec<Window>) -> Result<(), StreamAbort> {
        let mut held: Vec<Vec<u8>> = Vec::new();
        for window in windows {
            let span = self.read(window.start, window.end - window.start).await?;
            let mut ready = Vec::new();
            for (run, piece) in window.members {
                let from = usize::try_from(piece.offset - window.start).unwrap_or(usize::MAX);
                let to = from.saturating_add(usize::try_from(piece.length).unwrap_or(usize::MAX));
                let Some(bytes) = span.get(from..to) else {
                    return self.fail("source read was shorter than requested").await;
                };
                if run == 0 {
                    ready.extend_from_slice(bytes);
                } else {
                    if held.len() < run {
                        held.resize_with(run, Vec::new);
                    }
                    held[run - 1].extend_from_slice(bytes);
                }
            }
            if !ready.is_empty() {
                self.send(Ok(Bytes::from(ready))).await?;
            }
        }
        let chunk_size = self.chunk_size.max(1);
        for bytes in held {
            for chunk in bytes.chunks(chunk_size) {
                self.send(Ok(Bytes::copy_from_slice(chunk))).await?;
            }
        }
        Ok(())
    }

    /// The source byte ranges the requested interval needs after the header, in response order.
    fn requested_pieces(&mut self, header_end: u64) -> Vec<ByteRange> {
        let mut pieces = Vec::new();
        let mut virtual_offset = header_end;
        for source_range in std::mem::take(&mut self.prepared.ranges) {
            let source_end = virtual_offset + source_range.length;
            if let Some(overlap) = self.interval.overlap(virtual_offset, source_end) {
                pieces.push(ByteRange::new(
                    source_range.offset + overlap.start - virtual_offset,
                    overlap.end - overlap.start,
                ));
            }
            virtual_offset = source_end;
        }
        pieces
    }

    async fn read(&mut self, offset: u64, length: u64) -> Result<Bytes, StreamAbort> {
        let permit = match self.first_permit.take() {
            Some(permit) => permit,
            None => match acquire_segment_permit(&self.segment_jobs, self.queue_timeout).await {
                Ok(permit) => permit,
                Err(failure) => {
                    if matches!(failure, PermitFailure::Timeout) {
                        self.metrics.segment_queue_timeout();
                    }
                    return self.fail("segment read queue timed out").await;
                }
            },
        };
        let result = self.asset.read_range(ByteRange::new(offset, length)).await;
        drop(permit);
        match result {
            Ok(bytes) => {
                self.metrics.source_read_bytes(bytes.len());
                Ok(bytes)
            }
            Err(error) => {
                tracing::error!(event = "source_read_failed", %error);
                self.fail("source read failed").await
            }
        }
    }

    async fn send(&self, item: std::io::Result<Bytes>) -> Result<(), StreamAbort> {
        let length = item.as_ref().map_or(0, Bytes::len);
        match timeout(self.idle_timeout, self.sender.send(item)).await {
            Ok(Ok(())) => {
                self.metrics.response_bytes(length);
                Ok(())
            }
            Ok(Err(_)) => Err(StreamAbort::Client),
            Err(_) => {
                tracing::warn!(event = "stream_idle_timeout", "client stopped reading");
                Err(StreamAbort::Idle)
            }
        }
    }

    /// Surfaces a generic body error to the client, then reports the abort.
    async fn fail<T>(&self, message: &'static str) -> Result<T, StreamAbort> {
        let _ = self.send(Err(std::io::Error::other(message))).await;
        Err(StreamAbort::Error)
    }
}

/// The largest gap between two pieces that one read may span. Interleaved files put the other
/// track's frames between a track's samples, typically a few hundred bytes to a few KiB each.
const MAX_GAP_BYTES: u64 = 64 * 1024;

/// How many bytes the gaps of one segment may add to its reads in total: TDD 0001's budget of
/// "no more than payload bytes plus 256 KiB" read per segment.
const GAP_BUDGET_BYTES: u64 = 256 * 1024;

/// One source read and what it serves.
#[derive(Debug, PartialEq, Eq)]
enum Read {
    /// One piece longer than a chunk, read and sent a chunk at a time.
    Split(ByteRange),
    /// One read of `start..end` that serves every piece in it, in order.
    Span {
        start: u64,
        end: u64,
        pieces: Vec<ByteRange>,
    },
}

/// One read of `start..end` that serves the pieces in it, each tagged with the run of the
/// response it belongs to (0 for the first track's, 1 for the next, and so on).
#[derive(Debug, PartialEq, Eq)]
struct Window {
    start: u64,
    end: u64,
    members: Vec<(usize, ByteRange)>,
}

/// The most runs (tracks) read together, and, in chunks, the most bytes held back until the
/// first run is done. Audio is a fraction of a muxed segment; this keeps a pathological file
/// from holding a whole segment per request outside the job-slot accounting.
const MAX_RUNS: usize = 4;
const HELD_CHUNKS: u64 = 4;

/// A plan that reads each file region once for a segment made of several tracks interleaved in
/// the file, or `None` when [`coalesce`] already does as well.
///
/// A response lists one track's pieces, then the next's. Each track's pieces run forward through
/// the file and the next track's start back at the beginning, so [`coalesce`] reads the whole
/// region once per track, and in a muxed segment the other track's bytes make up most of every
/// gap. Here the pieces of all the runs are read together in file order, in windows of at most
/// `chunk_size`, and each piece is routed to its place in the response. Used only when it reads
/// fewer bytes than `coalesce` would, no piece is larger than a window, the runs are few, and
/// what must be held back is small.
fn interleaved_windows(pieces: &[ByteRange], chunk_size: u64) -> Option<Vec<Window>> {
    // A new run starts wherever a piece comes earlier in the file than the one before it.
    let mut tagged = Vec::with_capacity(pieces.len());
    let mut run = 0usize;
    let mut previous_end = 0u64;
    for &piece in pieces {
        if piece.offset < previous_end {
            run += 1;
        }
        previous_end = piece.offset + piece.length;
        tagged.push((run, piece));
    }
    let runs = run + 1;
    if !(2..=MAX_RUNS).contains(&runs) || pieces.iter().any(|piece| piece.length > chunk_size) {
        return None;
    }
    let held: u64 = tagged
        .iter()
        .filter(|(run, _)| *run != 0)
        .map(|(_, piece)| piece.length)
        .sum();
    if held > chunk_size.saturating_mul(HELD_CHUNKS) {
        return None;
    }

    tagged.sort_by_key(|(_, piece)| piece.offset);
    let mut windows: Vec<Window> = Vec::new();
    let mut gap_spent = 0u64;
    for (run, piece) in tagged {
        let end = piece.offset + piece.length;
        if let Some(window) = windows.last_mut() {
            let gap = piece.offset.saturating_sub(window.end);
            if gap <= MAX_GAP_BYTES
                && gap_spent + gap <= GAP_BUDGET_BYTES
                && end.max(window.end) - window.start <= chunk_size
            {
                gap_spent += gap;
                window.end = window.end.max(end);
                window.members.push((run, piece));
                continue;
            }
        }
        windows.push(Window {
            start: piece.offset,
            end,
            members: vec![(run, piece)],
        });
    }

    let merged: u64 = windows.iter().map(|window| window.end - window.start).sum();
    let separate: u64 = coalesce(pieces, chunk_size)
        .iter()
        .map(|read| match read {
            Read::Split(piece) => piece.length,
            Read::Span { start, end, .. } => end - start,
        })
        .sum();
    (merged < separate).then_some(windows)
}

/// Groups pieces into as few reads as the limits allow.
///
/// A segment's samples are usually interleaved in the file with another track's, so its byte
/// ranges are many small pieces with small gaps: a 6-second video segment of a typical file is
/// some 180 pieces of about 1 KiB. Reading each on its own costs a blocking-pool hop, a job slot,
/// and a channel send apiece. A piece joins the current read when it comes later in the file, the
/// gap before it is at most [`MAX_GAP_BYTES`], the read stays within `chunk_size`, and the
/// segment's total gap bytes stay within [`GAP_BUDGET_BYTES`].
fn coalesce(pieces: &[ByteRange], chunk_size: u64) -> Vec<Read> {
    let mut reads = Vec::new();
    let mut gap_spent = 0u64;
    let mut current: Option<(u64, u64, Vec<ByteRange>)> = None;
    for &piece in pieces {
        let end = piece.offset + piece.length;
        if let Some((start, current_end, members)) = current.as_mut() {
            let gap = piece.offset.checked_sub(*current_end);
            let fits = gap.is_some_and(|gap| {
                gap <= MAX_GAP_BYTES
                    && gap_spent + gap <= GAP_BUDGET_BYTES
                    && end - *start <= chunk_size
            });
            if fits {
                gap_spent += gap.unwrap_or(0);
                *current_end = end;
                members.push(piece);
                continue;
            }
        }
        if let Some((start, end, members)) = current.take() {
            reads.push(Read::Span {
                start,
                end,
                pieces: members,
            });
        }
        if piece.length > chunk_size {
            reads.push(Read::Split(piece));
        } else {
            current = Some((piece.offset, end, vec![piece]));
        }
    }
    if let Some((start, end, members)) = current {
        reads.push(Read::Span {
            start,
            end,
            pieces: members,
        });
    }
    reads
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piece(offset: u64, length: u64) -> ByteRange {
        ByteRange::new(offset, length)
    }

    #[test]
    fn interleaved_pieces_become_one_read() {
        // A video sample of 1000 bytes, then 300 bytes of another track, repeating.
        let pieces = (0..100).map(|n| piece(n * 1300, 1000)).collect::<Vec<_>>();

        let reads = coalesce(&pieces, 256 * 1024);

        assert_eq!(reads.len(), 1);
        let Read::Span {
            start,
            end,
            pieces: members,
        } = &reads[0]
        else {
            panic!("a span");
        };
        assert_eq!((*start, *end), (0, 99 * 1300 + 1000));
        assert_eq!(members.len(), 100);
    }

    #[test]
    fn a_read_never_exceeds_the_chunk_size() {
        let pieces = (0..100).map(|n| piece(n * 1300, 1000)).collect::<Vec<_>>();

        let reads = coalesce(&pieces, 10_000);

        for read in &reads {
            let Read::Span { start, end, .. } = read else {
                panic!("small pieces stay spans");
            };
            assert!(end - start <= 10_000);
        }
        let served: usize = reads
            .iter()
            .map(|read| match read {
                Read::Span { pieces, .. } => pieces.len(),
                Read::Split(_) => 1,
            })
            .sum();
        assert_eq!(served, 100, "every piece is served exactly once");
    }

    #[test]
    fn large_gaps_backward_pieces_and_the_gap_budget_split_reads() {
        // A gap over the limit starts a new read.
        assert_eq!(
            coalesce(&[piece(0, 10), piece(10 + MAX_GAP_BYTES + 1, 10)], u64::MAX).len(),
            2
        );
        // A piece earlier in the file than the one before it is never joined to it.
        assert_eq!(
            coalesce(&[piece(1000, 10), piece(0, 10)], u64::MAX).len(),
            2
        );
        // Gaps add up: once the segment's budget is spent, pieces are read on their own.
        let gap = MAX_GAP_BYTES;
        let pieces = (0..10)
            .map(|n| piece(n * (gap + 10), 10))
            .collect::<Vec<_>>();
        let reads = coalesce(&pieces, u64::MAX);
        let spent = usize::try_from(GAP_BUDGET_BYTES / gap).unwrap();
        assert_eq!(reads.len(), 10 - spent, "{reads:?}");
    }

    #[test]
    fn a_piece_larger_than_a_chunk_is_read_in_chunks() {
        let reads = coalesce(&[piece(0, 10), piece(10, 1000), piece(1010, 10)], 100);

        assert_eq!(reads[1], Read::Split(piece(10, 1000)));
        assert_eq!(reads.len(), 3);
    }

    /// A video track and an audio track alternating in the file, as the response lists them:
    /// all the video pieces, then all the audio pieces.
    fn muxed_pieces(frames: u64, video: u64, audio: u64) -> Vec<ByteRange> {
        let stride = video + audio;
        (0..frames)
            .map(|n| piece(n * stride, video))
            .chain((0..frames).map(|n| piece(n * stride + video, audio)))
            .collect()
    }

    #[test]
    fn interleaved_tracks_are_read_once_instead_of_once_per_track() {
        let pieces = muxed_pieces(100, 1000, 300);

        let windows = interleaved_windows(&pieces, 256 * 1024).expect("a muxed segment");

        let read: u64 = windows.iter().map(|window| window.end - window.start).sum();
        assert_eq!(read, 100 * 1300, "every file byte once, nothing skipped");
        let separate: u64 = coalesce(&pieces, 256 * 1024)
            .iter()
            .map(|read| match read {
                Read::Span { start, end, .. } => end - start,
                Read::Split(piece) => piece.length,
            })
            .sum();
        assert!(read < separate, "{read} against {separate}");
        // Every piece is served once, tagged with its track.
        let mut members = windows
            .iter()
            .flat_map(|window| window.members.iter().copied())
            .collect::<Vec<_>>();
        members.sort_by_key(|(_, piece)| piece.offset);
        assert_eq!(members.len(), 200);
        assert_eq!(members.iter().filter(|(run, _)| *run == 0).count(), 100);
        assert_eq!(members.iter().filter(|(run, _)| *run == 1).count(), 100);
    }

    #[test]
    fn windows_respect_the_chunk_size_and_never_split_a_piece() {
        let pieces = muxed_pieces(100, 1000, 300);

        let windows = interleaved_windows(&pieces, 10_000).expect("a muxed segment");

        assert!(windows.len() > 10);
        for window in &windows {
            assert!(window.end - window.start <= 10_000);
            for (_, member) in &window.members {
                assert!(member.offset >= window.start);
                assert!(member.offset + member.length <= window.end);
            }
        }
    }

    #[test]
    fn a_single_run_or_a_plan_that_saves_nothing_is_left_to_coalesce() {
        // One track alone: pieces only move forward.
        let video = (0..100).map(|n| piece(n * 1300, 1000)).collect::<Vec<_>>();
        assert_eq!(interleaved_windows(&video, 256 * 1024), None);
        // Two tracks in different regions of the file: reading together saves nothing.
        let apart = [piece(0, 1000), piece(1_000_000, 1000), piece(500_000, 1000)];
        assert_eq!(interleaved_windows(&apart, 256 * 1024), None);
        // A piece larger than a window.
        assert_eq!(interleaved_windows(&muxed_pieces(10, 1000, 300), 900), None);
    }

    #[test]
    fn too_much_to_hold_back_is_left_to_coalesce() {
        // The second track is most of the segment: 100 pieces of 5,000 bytes against a 10 KB
        // chunk size, so the held-back part would be fifty chunks.
        let pieces = muxed_pieces(100, 100, 5000);
        assert_eq!(interleaved_windows(&pieces, 10_000), None);
    }

    #[test]
    fn contiguous_pieces_join_with_no_gap_cost() {
        let pieces = (0..10).map(|n| piece(n * 100, 100)).collect::<Vec<_>>();

        assert_eq!(
            coalesce(&pieces, u64::MAX),
            vec![Read::Span {
                start: 0,
                end: 1000,
                pieces
            }]
        );
    }
}
