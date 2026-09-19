//! The `vod-module-rs` binary: a thin wrapper around the library's [`run`](vod_module_rs::run).

#![forbid(unsafe_code)]

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    vod_module_rs::run().await
}
