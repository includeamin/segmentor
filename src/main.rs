//! The `segmentor` binary: a thin wrapper around the library's [`run`](segmentor::run).

#![forbid(unsafe_code)]

use std::process::ExitCode;

/// mimalloc instead of the system allocator. Loading an asset allocates and frees several
/// megabytes of tables on whichever threads did the work; glibc returns large freed blocks to the
/// kernel and keeps per-thread arenas apart, so every load faults fresh memory in again.
/// mimalloc reuses it: a cold load is about 20 % faster, for about 35 MB more resident memory
/// (docs/benchmarks.md, "What it found: a slow first request").
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> ExitCode {
    segmentor::run().await
}
