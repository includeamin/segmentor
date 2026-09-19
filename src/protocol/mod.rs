//! Streaming-protocol renderers: HLS playlists and the DASH manifest.
//!
//! They are pure functions from a [`Presentation`] to text, with no I/O and no knowledge of the
//! HTTP layer or the loaded asset.

pub(crate) mod dash;
pub(crate) mod hls;
mod presentation;

pub(crate) use presentation::Presentation;
#[cfg(test)]
pub(crate) use presentation::fixtures;
