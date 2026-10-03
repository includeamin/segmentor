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

Recorded 2026-10-02 with `make bench` on a laptop: Intel Core i7-8550U (4 cores, 8 threads, 1.8 GHz base), 15 GiB RAM, Linux, warm page cache, four Tokio workers, one release build.

| Measurement | Budget | Result |
| --- | --- | --- |
| Warm load, 60-minute asset | p95 < 250 ms | p50 94 ms, p95 110 ms |
| Cold first load | - | 137 ms |
| 6 s header generation | p95 < 10 ms | p95 0.008 ms |
| HLS master / HLS media / DASH (loopback, includes client) | p95 < 2 ms | p95 0.34 / 0.35 / 0.14 ms |
| Full 6 s segment over loopback | - | p50 1.7 ms, p95 2.1 ms |
| Source bytes beyond payload | <= 256 KiB | 67 KiB per segment, from reading across small gaps between a track's samples |
| 1,000 sustained streams | < 0.1 % errors | 0 of 71,064 requests; 7,041 req/s, 1,367 MiB/s |
| Extra memory per streaming connection | <= 512 KiB | 101 KiB (includes the benchmark's own client buffers) |

The figures were refreshed after segment reads began coalescing nearby byte ranges (see [What it found](#what-it-found-one-read-per-sample)): throughput rose about tenfold, at the cost of reading some bytes between a track's samples. Earlier, after the async source and registry refactor, the load path now fetches metadata through `Metadata` and assembles on the blocking pool, which costs a few tens of milliseconds more than the earlier synchronous path but stays well inside the budget. The 60-minute asset has 600 segments and an index of 8.6 MiB (about 276,000 samples).

## A finding the harness caught

The first run failed the startup budget by roughly ten times: a warm load of the 60-minute asset took about 2.4 seconds. The cause was that the `mp4` crate reads every sample-table entry with its own `read` call, and the parser handed it an unbuffered `File`, so parsing issued millions of system calls (the run also showed high system CPU). `LocalMediaSource::parser_file` now returns a 256 KiB `BufReader`. The same load takes about 70 ms, a 34-fold improvement, and initialization-segment generation, which parses again per track, benefits equally. Sample-table entries are still read one at a time in user space, so a future optimization could feed the crate an in-memory `ftyp` and `moov` instead.

## Limits of these results

- **One host class.** The budgets name a documented four-core x86-64 host with local SSD. This laptop is comparable but not identical; record the CPU, storage, and command when comparing runs, and run on the reference host before declaring the budgets met for release.
- **Loopback, shared runtime.** Client and server run in one process on one runtime, so client work competes with the server for the four workers. That makes the results conservative for throughput and latency, and it means the playlist latencies include client parsing rather than pure server time.
- **One asset, warm cache.** All streams read the same file from the page cache. A large catalog on cold storage will be I/O-bound, which the source-read counters will show.
- **Memory is a coarse estimate.** Peak resident size divided by streams includes the benchmark client's buffers and allocator behavior. The design bound is two channel items of `stream_chunk_bytes` (512 KiB at defaults) per stream.
- **Event-loop blocking is not directly measured.** The requirement that no filesystem operation on a Tokio worker exceeds 1 ms is met structurally: source reads and header construction run on the blocking pool. The canary's scheduling delay is reported for information only, because CPU saturation from the client on the same runtime inflates it (p99 about 1.6 ms, one outlier of about 13 ms).
- **No packet-level tail under sustained overload.** The streaming test runs at the configured concurrency for ten seconds and does not probe behavior past the limits; see the shedding and connection-cap tests for that.

## segmentor vs nginx-vod-module

The budgets above say whether segmentor is fast enough. This comparison says how it stands against what it is meant to replace: both servers serve the same file, one at a time, on the same pinned cores, driven by the same external load generator.

### Running it

```sh
make bench-compare                      # every variant, 64 virtual users, 30 s per scenario
VUS=256 DURATION=60s make bench-compare
SERVERS="segmentor nginx-cached" make bench-compare
```

It also runs on GitHub Actions: the `Benchmarks` workflow, run by hand from the Actions tab with `job: compare`, puts the report in the job summary and the raw results in an artifact. A hosted runner has 4 shared vCPUs, so servers get 2 and k6 the other 2; absolute figures vary from run to run there, but every server in one run is measured on the same machine, so the comparison between them holds.

It needs Docker. Everything lives in [`bench/compare/`](https://github.com/includeamin/segmentor/tree/main/bench/compare):

- `nginx-vod-module/` builds nginx 1.26.3 with nginx-vod-module 1.33 from their upstream release tarballs. Nothing from nginx-vod-module (AGPL-3.0) is copied into segmentor; it is built and run as a separate program.
- `segmentor.toml` and `nginx-vod-module/nginx.conf` configure the same work for both (below).
- `k6/load.js` crawls each server's master playlist to find every playlist and segment, so one script drives both URL schemes.
- `run.sh` runs the scenarios and writes `results/<timestamp>/report.md`.

### What is made equal

| Setting | Both servers |
| --- | --- |
| Input | The 60-minute H.264/AAC asset from the budget benchmark, read from local disk |
| Output | HLS with fMP4 segments (nginx-vod-module's default is MPEG-TS, so `vod_hls_container_format fmp4`) and relative URLs |
| Segmentation | 6-second target, keyframe-aligned (`vod_align_segments_to_key_frames on`), exact durations in playlists (`vod_manifest_segment_durations_mode accurate`) |
| Caching | Parsed metadata cached; no segment cache. nginx-vod-module is measured twice: with its response cache off (`nginx`), and on (`nginx-cached`, its best case for playlists) |
| Audio | Both mux audio into the video segments by default. segmentor is also measured with audio as its own rendition (`segmentor-separate`, `packaging.hls_mux_audio = false`, [TDD 0011](technical-design/0011-muxed-hls-audio.md)) |
| Compression | Playlists compressed when the client accepts it: nginx-vod-module with `gzip on` for playlist types, as its README recommends; segmentor with brotli or gzip. k6 sends `Accept-Encoding: gzip, deflate, br`, as browsers do |
| Cold start | Both parse an asset on its first request (segmentor with `registry.preload = false`) |
| CPU | Server pinned to cores 0-3 with 4 workers (`worker_processes 4`, `TOKIO_WORKER_THREADS=4`); k6 pinned to cores 4-7 |

With audio as its own rendition, a presentation is 1,202 requests from segmentor; muxed, it is 601, and 615 from nginx-vod-module (which cuts 14 more segments), all for the same 160 MiB and the same 3,680 seconds. Compare throughput in MiB/s and MiB/s per core, and playlists per viewer, not raw requests per second. Before each report, `run.sh` checks that both presentations decode with FFmpeg and have the same duration and payload.

### Scenarios

| Scenario | What it measures |
| --- | --- |
| Cold start | A fresh process each time: time to the first master playlist, then a media playlist, then a segment. Median of 5 |
| Cold asset, running process | Then, in that running process, the same for five assets it has never loaded: hard links to the same file under other names, so only the servers' own caches are cold. This is the usual production case: a long-tail asset's first viewer rather than a restart |
| Manifests | Random master and media playlist requests from 64 virtual users for 30 s. Bodies are discarded, so k6 spends no CPU parsing them; bytes on the wire are still counted |
| Segments | Random init and media segments from 64 virtual users for 30 s, with server CPU and memory sampled from `docker stats` |

### Results

Recorded 2026-10-03 on an Intel Core i7-8550U (4 cores, 8 threads), 15 GiB RAM, Linux, Docker 29, warm page cache, k6 0.57, CPU governor `powersave`. Servers on 4 cores and k6 on the other 4, except where marked "1 core": servers on 1 core and k6 on 7, so that the server is the limit and not the load generator.

| Measurement | segmentor, separate audio | segmentor (muxed, the default) | nginx-vod-module | nginx-vod-module, response cache | Best against best |
| --- | ---: | ---: | ---: | ---: | --- |
| Segments: throughput | 1,019 MiB/s | 1,001 MiB/s | 473 MiB/s | 474 MiB/s | segmentor 2.1× |
| Segments, 1 core: throughput | 444 MiB/s | 447 MiB/s | 197 MiB/s | 192 MiB/s | segmentor 2.3× |
| Segments: latency p50 / p99 | 7.9 / 21.5 ms | 16.8 / 23.7 ms | 35.1 / 53.7 ms | 35.1 / 53.3 ms | muxed segments are twice the size |
| Segments: peak memory | 103 MiB | 94 MiB | 136 MiB | 130 MiB | |
| Playlists: requests/s | 22,910 | 22,986 | 506 | 21,087 | k6-bound at 4 cores; see 1 core |
| Playlists: bytes on the wire | 20.5 MiB/s | 17.6 MiB/s | 0.6 MiB/s | 25.4 MiB/s | |
| Playlists, 1 core: requests/s | 17,603 | 17,108 | 174 | 12,988 | segmentor 1.36× |
| Playlists, 1 core: a viewer's full set per second | 5,868 | 8,554 | 87 | 6,494 | segmentor muxed 1.32× |
| Cold start: first master playlist | 6.6 ms | 6.7 ms | 4.6 ms | 4.6 ms | nginx-vod-module 1.4× faster |
| Cold start: master, media playlist, and first segment | 8.5 ms | 8.9 ms | 19.4 ms | 19.7 ms | segmentor 2.3× faster |
| Errors | 0 | 0 | 0 | 0 | |

Segment throughput varies by about 10 % between runs on this laptop. Under `powersave`, a fresh process starts on cores that are clocked down, so cold-start figures are higher than on a server with the `performance` governor. That affects both servers.

What this shows:

- **Segments, the bulk of origin traffic:** segmentor serves twice the bandwidth on the same cores, at lower latency and with less memory.
- **Playlists:** segmentor renders a playlist once, on its first request, and stores it with its brotli and gzip forms. nginx-vod-module regenerates playlists per request unless its response cache is on; with exact durations over a 600-segment playlist that costs about 130 ms each. With the cache on, segmentor still serves 1.36 times the requests on one core. With audio as its own rendition a viewer needs one more playlist from segmentor, so per viewer nginx-vod-module is ahead (6,494 sets a second against 5,868); muxed, segmentor is 1.32 times ahead. Either way, a CDN in front caches playlists for both.
- **Cold start:** the first viewer gets a segment in less than half the time from segmentor. nginx-vod-module still answers the master playlist alone sooner, because it defers parsing to the media playlist, while segmentor builds the whole index first. The next section has the detail.

### What it found: a slow first request

The first comparison had the first viewer of the 60-minute asset waiting 245 ms on segmentor for the master playlist, media playlist, and first segment, against 69 ms on nginx-vod-module. Measured in process, loading the asset took about 64 ms. Most of that was parsing, and most of the parsing was hashing:

| Change | Load in process | Where |
| --- | ---: | --- |
| Starting point | ~64 ms | |
| Hash `moov` once, not three times; the mutation check compares bytes instead of hashing again | ~32 ms | `mp4::parser` |
| BLAKE3 instead of SHA-256 for the content hash (1.1 ms instead of 18 ms for a 3.8 MB `moov` on this CPU, which has no SHA instructions) | ~17 ms | `mp4::parser` |
| A flag per sample instead of a hash set for `stss` | ~15 ms | `mp4::tables` |
| Render the master playlist at load and every other playlist on its first request | ~12.5 ms | `asset::RenderedManifests` |
| Write each table straight into the one sample list instead of four intermediate arrays | ~12 ms | `mp4::tables::expand_samples` |
| Expand tracks in parallel, with the hash alongside | ~8.5 ms | `mp4::parser::parse_tracks` |
| Keep the tables compact instead of a record per sample ([TDD 0010](technical-design/0010-compact-sample-index.md)); give every track its own thread; re-read `moov` for the mutation check while parsing | ~4 ms | `media::SampleIndex`, `mp4::tables::sample_index` |
| mimalloc as the global allocator | about 20 % less, fresh process and running process alike | `src/main.rs` |

Measured end to end, a fresh container's first master playlist went from 242 ms to 6.7 ms over these changes, and the index of the 60-minute asset from 8.9 to 4.4 MB. What is left is mostly the fresh process itself: starting threads, faulting in memory, and the CPU ramping up from idle under `powersave`; a load in an already-running process takes about 4 ms. The content hash is part of every URL's `?v=` version, so the switch to BLAKE3 changes every asset URL once. The version also covers the format revision, and nothing else about the output changed.

The allocator change came from counting page faults, which, unlike timings, the laptop's background load cannot disturb. A cold load touched about 10 MB of fresh memory: the 4.4 MB index it keeps, plus the 3.8 MB `moov` copy and the intermediate tables it frees. With glibc, that held even for a second asset in a running process, because glibc returns large freed blocks to the kernel and keeps each thread's arena to itself, and a load's work runs on whichever blocking-pool and track threads are free. Measured over 12 fresh processes alternating between the two builds, mimalloc took the first master from a median of 25.8 to 20.4 ms, and a second asset's from 21.6 to 17.9 ms (on a busy laptop, hence the high absolute figures). It keeps about 35 MB more resident: 51 MB against 16 MB after one asset, 80 MB against 38 MB after four, a fixed cost rather than one per asset.

### What it found: one read per sample

The first run had segmentor at 52 MiB/s against nginx-vod-module's 449, more than 8 times slower, using 2.4 cores to do it. The cause was in segmentor, not the harness. A segment's bytes are a list of byte ranges, one per contiguous run of samples. In ordinary encoder output, audio and video are interleaved frame by frame, so a 6-second video segment was 175 ranges of about 1 KiB. Each was its own blocking-pool read, job slot, and channel send.

The streaming task now groups nearby ranges into one read of up to `limits.stream_chunk_bytes` and copies the samples out of it (`src/http/stream.rs`, `coalesce`). A gap between ranges joins a read only if it is at most 64 KiB, and the gaps added to one segment's reads stay within TDD 0001's budget of 256 KiB beyond the payload. Segment throughput went from 52 to 692 MiB/s and median latency from 80 to 10 ms. The fixtures behind the budget benchmark are interleaved in the same way, so earlier budget runs measured this cost too.

### What it found: playlists sent uncompressed

With its response cache on, nginx-vod-module answered 20,356 playlist requests a second against segmentor's 15,253. segmentor was using 1.5 of its 4 cores at the time; the load generator was using 3.4 of its 4. Two harness changes made the server the thing measured. k6 now discards response bodies, and a 1-core run gives k6 seven cores to the server's one.

That showed the real difference: bytes. nginx-vod-module gzips playlists, as its README recommends, and segmentor sent them uncompressed: 34 KB for a one-hour media playlist, against 1.7 KB gzipped. segmentor now keeps a brotli and a gzip form of each playlist, made once on its first request (`protocol::Manifest`), and sends the best one the client accepts. The same playlist is 857 bytes with brotli. Playlist bandwidth went from 495 MiB/s to 20 MiB/s at the same request rate.

Moving the URL version from a `?v=` on every segment URI into the path, which would shorten playlists, is not worth the URL change any more: the repeated `?v=` compresses to almost nothing.

### What it found: muxed segments read the file twice

The first run on a GitHub-hosted runner (servers on 2 cores) had muxed segmentor at 585 MiB/s against 764 MiB/s with audio as its own rendition, and 2 cores on a laptop reproduced it: 488 against 572. CPU was not the limit (143 % of 200 %). Counting bytes read from the file against bytes served showed why. A muxed response lists the video pieces, then the audio pieces; the audio pieces start back at the beginning of the same region, so the streaming task read that region once for each track, 1.89 times the segment's size, with each track's reads spanning the other's bytes. Read one after another, a muxed request did the work of two.

`interleaved_windows` (`src/http/stream.rs`) now reads such a region once, in windows of at most `stream_chunk_bytes`, sends the video pieces from each window as it arrives, and holds the audio pieces (about a quarter of a segment) until the video is done, so the response bytes are unchanged. It is used only when it reads fewer bytes than the old plan, no piece is larger than a window, and the held-back part is at most four chunks. A muxed segment now reads 0.97 times its size, and 2-core throughput went from 488 to 770–825 MiB/s, faster than separate audio (493–533 MiB/s), which reads 1.33 times for video and 3.34 times for audio.

### Limits of this comparison

- **One host, loopback.** The client and server share a machine on separate cores. Real deployments have network latency, TLS, and a CDN in front.
- **One asset, warm page cache.** It does not measure many assets competing for memory, cold storage, or remote (HTTP) sources, where nginx-vod-module and segmentor read very differently.
- **Clear content only.** Encryption is per request in segmentor and is not measured here yet.
- **Default tuning for both.** Each server uses its own documented recommendations, not an expert tuning pass.

## Regressions

`make bench-enforce` exits non-zero when a budget is missed. The `Benchmarks` workflow runs it on demand from the Actions tab (`workflow_dispatch`), because shared CI runners are too noisy to gate every pull request on latency budgets.
