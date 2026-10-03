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
