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
/// A job slot is held only while a source read is in flight, never while waiting for the
/// client, so slow readers cannot exhaust `max_segment_jobs`. Each send is bounded by the idle
/// timeout, so a client that stops reading is dropped instead of pinning the task.
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

    async fn stream(&mut self) -> std::result::Result<(), StreamAbort> {
        let header_end = self.prepared.header.len() as u64;
        if let Some(overlap) = self.interval.overlap(0, header_end) {
            let (Ok(start), Ok(end)) =
                (usize::try_from(overlap.start), usize::try_from(overlap.end))
            else {
                return self.fail("header range does not fit in memory").await;
            };
            let chunk = self.prepared.header.slice(start..end);
            self.send(Ok(chunk)).await?;
        }
        let mut virtual_offset = header_end;
        for source_range in std::mem::take(&mut self.prepared.ranges) {
            let source_end = virtual_offset + source_range.length;
            let Some(overlap) = self.interval.overlap(virtual_offset, source_end) else {
                virtual_offset = source_end;
                continue;
            };
            let mut offset = source_range.offset + overlap.start - virtual_offset;
            let mut remaining = overlap.end - overlap.start;
            while remaining != 0 {
                let length = remaining.min(self.chunk_size as u64);
                let chunk = self.read(offset, length).await?;
                self.send(Ok(chunk)).await?;
                offset += length;
                remaining -= length;
            }
            virtual_offset = source_end;
        }
        Ok(())
    }

    async fn read(&mut self, offset: u64, length: u64) -> std::result::Result<Bytes, StreamAbort> {
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
        let asset = Arc::clone(&self.asset);
        let result =
            tokio::task::spawn_blocking(move || asset.read_range(ByteRange::new(offset, length)))
                .await;
        drop(permit);
        match result {
            Ok(Ok(bytes)) => {
                self.metrics.source_read_bytes(bytes.len());
                Ok(bytes)
            }
            Ok(Err(error)) => {
                tracing::error!(event = "source_read_failed", %error);
                self.fail("source read failed").await
            }
            Err(error) => {
                tracing::error!(event = "source_read_failed", %error);
                self.fail("source read failed").await
            }
        }
    }

    async fn send(&self, item: std::io::Result<Bytes>) -> std::result::Result<(), StreamAbort> {
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
    async fn fail<T>(&self, message: &'static str) -> std::result::Result<T, StreamAbort> {
        let _ = self.send(Err(std::io::Error::other(message))).await;
        Err(StreamAbort::Error)
    }
}
