# Performance budgets

The budgets in [TDD 0001](technical-design/0001-on-demand-mp4-packaging-core.md#initial-performance-budgets) are measured by a harness that runs the real server and the real packaging code. This page records how to run it, what it measures, the current results, and what the results do and do not show.

## Running it

```sh
make bench            # print the results
make bench-enforce    # exit non-zero if a budget is missed
```

The harness is `benches/budgets.rs` (`harness = false`, no benchmarking dependency). It starts the server in-process on a loopback port through the hidden `benchmarking` module and drives it with a small keep-alive HTTP client. `cargo test` skips it, so it never runs as part of the test suite.

On the first run it uses FFmpeg to synthesize a 60-minute H.264/AAC asset in `target/bench/` by stream-copying the committed 3-second fixture 1,200 times (no re-encoding, `-use_editlist 0`). Without FFmpeg the long-asset checks are skipped.

| Variable | Default | Meaning |
| --- | ---: | --- |
| `BUDGET_STREAMS` | 1000 | Concurrent streaming clients |
| `BUDGET_SECONDS` | 10 | Duration of the streaming test |
| `BUDGET_WORKERS` | 4 | Tokio worker threads, mirroring the four-core reference host |

## What is measured

| Measurement | Budget | How |
| --- | --- | --- |
| Warm load (open, parse, plan, init segments, render) of a 60-minute asset | p95 < 250 ms | 11 repeated loads after one discarded cold load |
| Warm 6 s segment header generation | p95 < 10 ms | 3,000 `prepare_media_segment` calls across the timeline |
| Cached HLS master, HLS media, and DASH responses | p95 < 2 ms | 3,000 sequential requests each over one keep-alive connection |
| Source bytes read beyond the payload | <= 256 KiB per segment | `vod_source_read_bytes_total` against bytes sent, over 300 segments |
| Origin 5xx and connection errors with 1,000 sustained streams | < 0.1 % | 1,000 connections each fetching random segments for the test duration |
| Extra resident memory per streaming connection | <= 512 KiB | Peak RSS minus idle RSS, divided by the stream count |

It also reports, without a budget: full-segment transfer latency, throughput, requests shed or rejected, and the scheduling delay of a 1 ms canary task.

## Results

Recorded 2026-09-20 with `make bench` on a laptop: Intel Core i7-8550U (4 cores, 8 threads, 1.8 GHz base), 15 GiB RAM, Linux, warm page cache, four Tokio workers, one release build.

| Measurement | Budget | Result |
| --- | --- | --- |
| Warm load, 60-minute asset | p95 < 250 ms | p50 85 ms, p95 121 ms |
| Cold first load | - | 110 ms |
| 6 s header generation | p95 < 10 ms | p95 0.006 ms |
| HLS master / HLS media / DASH (loopback, includes client) | p95 < 2 ms | p95 0.071 / 0.075 / 0.073 ms |
| Full 6 s segment over loopback | - | p50 2.4 ms, p95 3.0 ms |
| Source bytes beyond payload | <= 256 KiB | 0 |
| 1,000 sustained streams | < 0.1 % errors | 0 of 6,302 requests; 598 req/s, 116 MiB/s |
| Extra memory per streaming connection | <= 512 KiB | 66 KiB (includes the benchmark's own client buffers) |

The figures were refreshed after the async source and registry refactor; the load path now fetches metadata through `Metadata` and assembles on the blocking pool, which costs a few tens of milliseconds more than the earlier synchronous path but stays well inside the budget. The 60-minute asset has 600 segments and an index of 8.6 MiB (about 276,000 samples).

## A finding the harness caught

The first run failed the startup budget by roughly ten times: a warm load of the 60-minute asset took about 2.4 seconds. The cause was that the `mp4` crate reads every sample-table entry with its own `read` call, and the parser handed it an unbuffered `File`, so parsing issued millions of system calls (the run also showed high system CPU). `LocalMediaSource::parser_file` now returns a 256 KiB `BufReader`. The same load takes about 70 ms, a 34-fold improvement, and initialization-segment generation, which parses again per track, benefits equally. Sample-table entries are still read one at a time in user space, so a future optimization could feed the crate an in-memory `ftyp` and `moov` instead.

## Limits of these results

- **One host class.** The budgets name a documented four-core x86-64 host with local SSD. This laptop is comparable but not identical; record the CPU, storage, and command when comparing runs, and run on the reference host before declaring the budgets met for release.
- **Loopback, shared runtime.** Client and server run in one process on one runtime, so client work competes with the server for the four workers. That makes the results conservative for throughput and latency, and it means the playlist latencies include client parsing rather than pure server time.
- **One asset, warm cache.** All streams read the same file from the page cache. A large catalog on cold storage will be I/O-bound, which the source-read counters will show.
- **Memory is a coarse estimate.** Peak resident size divided by streams includes the benchmark client's buffers and allocator behavior. The design bound is two channel items of `stream_chunk_bytes` (512 KiB at defaults) per stream.
- **Event-loop blocking is not directly measured.** The requirement that no filesystem operation on a Tokio worker exceeds 1 ms is met structurally: source reads and header construction run on the blocking pool. The canary's scheduling delay is reported for information only, because CPU saturation from the client on the same runtime inflates it (p99 about 1.6 ms, one outlier of about 13 ms).
- **No packet-level tail under sustained overload.** The streaming test runs at the configured concurrency for ten seconds and does not probe behavior past the limits; see the shedding and connection-cap tests for that.

## Regressions

`make bench-enforce` exits non-zero when a budget is missed. The `Benchmarks` workflow runs it on demand from the Actions tab (`workflow_dispatch`), because shared CI runners are too noisy to gate every pull request on latency budgets.
