//! The `segmentor` binary: a thin wrapper around the library's [`run`](segmentor::run).

#![forbid(unsafe_code)]

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    segmentor::run().await
}
