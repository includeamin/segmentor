//! `segmentor`: a video-on-demand origin that packages MP4 files as HLS and DASH on demand.
//!
//! The crate is a library plus a thin binary. The library's public surface is deliberately
//! small: [`run`] is the binary's entry point, and hidden helper modules expose the media pipeline
//! to the fuzz target and the benchmark harness. Everything else is crate-private; see the implementation guide in `docs/` for a
//! module-by-module description.

#![forbid(unsafe_code)]

use std::process::ExitCode;

mod asset;
mod cli;
mod config;
mod error;
mod fmp4;
mod http;
mod media;
mod mp4;
mod observability;
mod protocol;
mod registry;
mod resolver;
mod segment;
mod source;
mod subtitle;
#[cfg(test)]
mod testutil;

#[doc(hidden)]
pub mod benchmarking;
pub mod fuzzing;

const APP_NAME: &str = "segmentor";

/// Runs the command-line application with the process arguments.
///
/// Prints `segmentor: <error>` to standard error and returns a failure exit code if the
/// command fails.
pub async fn run() -> ExitCode {
    match cli::run(std::env::args_os().skip(1)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{APP_NAME}: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::APP_NAME;

    #[test]
    fn application_name_is_stable() {
        assert_eq!(APP_NAME, "segmentor");
    }
}
