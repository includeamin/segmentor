# segmentor

**On-demand HLS and DASH packaging for MP4 files, in Rust.**

[![CI](https://github.com/includeamin/segmentor/actions/workflows/ci.yml/badge.svg)](https://github.com/includeamin/segmentor/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

segmentor is an HTTP origin. Point it at MP4 files, on disk or on an HTTP origin that a small
*mapper* service tells it about, and it serves them as HLS and DASH, packaging each segment when
it is requested. Nothing is pre-generated, transcoded, or stored: the encoded audio and video are
re-wrapped as fragmented MP4 and streamed straight from the source, so the work per request is
proportional to the segment asked for, not to the length of the video.

> **Alpha (0.x).** The code is tested and documented, but it has been checked mostly against
> synthetic files and one browser, APIs and configuration may change between releases, and it has
> not been through a security review. Try it, tell us which files it does not handle, and read
> [what was and was not verified](docs/technical-design/0004-broader-mp4-input-support.md) before
> you depend on it.

## What it does

- **Packages on demand.** Playlists and manifests are built when an asset loads and segments when
  they are requested, from a sample index kept in memory. There is no packaging step and no output
  to store.
- **Reads what encoders produce.** Progressive and fragmented MP4, M4A, and QuickTime files; H.264,
  HEVC, VP9, and AV1; AAC, HE-AAC, AC-3, E-AC-3, Opus, and FLAC; edit lists, several audio tracks,
  and audio-only files. See [Supported input](#supported-input).
- **Finds media through a mapper.** A mapper service answers "where is asset X?" with a file or an
  HTTP location, so the catalog lives wherever you already keep it. Remote origins are read with
  ranged requests and the server never downloads a whole file.
- **Is bounded by construction.** Every length read from a file or a mapper is checked before it is
  allocated, and the limits are configuration. `unsafe` code is forbidden. The parser is run against
  corrupted input on every test run, and has a fuzz target.
- **Is built to be operated.** Prometheus metrics, request IDs, readiness and liveness endpoints,
  load shedding, graceful shutdown, immutable content-versioned URLs for CDNs, and byte-range and
  conditional requests.

## What it does not do

- **No transcoding.** The codecs must already suit the protocol, and there is no bitrate ladder
  unless you supply the renditions. Adaptive renditions, WebVTT subtitles, and HLS I-frame
  playlists are [planned](docs/technical-design/0006-trick-play-subtitles-and-renditions.md).
- **No DRM, no live streaming, no MPEG-TS output.** Segments are fragmented MP4. DRM is planned
  after the features above.
- **No TLS or authentication.** Run it behind a reverse proxy or CDN that provides both; see
  [operations](docs/operations.md).

## How it fits with other tools

| If you want to... | Consider |
| --- | --- |
| Convert files to HLS/DASH once and host static files | FFmpeg, [Shaka Packager](https://github.com/shaka-project/shaka-packager), or Bento4 |
| Serve HLS/DASH without storing packaged output, as a standalone service, with strict resource limits | **segmentor** |
| DRM, live streaming, or MPEG-TS output today | Shaka Packager |
| A mature on-demand packager that runs inside nginx | Kaltura's [nginx-vod-module](https://github.com/kaltura/nginx-vod-module) (AGPL-3.0) |

segmentor takes its idea, packaging on demand instead of ahead of time, from nginx-vod-module but
contains none of its code; see [the research notes](docs/research/nginx-vod-module.md). Check each
project's own documentation for what it supports today; this table is a rough guide.

## Quick start

```sh
git clone https://github.com/includeamin/segmentor
cd segmentor
make serve        # serves tests/fixtures/h264-aac.mp4 as the asset "sample" on :3000
```

Then play it:

```sh
ffplay http://127.0.0.1:3000/hls/sample/master.m3u8
```

or, in a second terminal, `make demo` and open <http://127.0.0.1:8080> for a player with live
server metrics next to it. `make help` lists everything else.

## Install

- **Release binaries.** Each [GitHub release](https://github.com/includeamin/segmentor/releases)
  attaches Linux binaries for x86-64 and arm64 with checksums and build attestations.
- **Container image.** `ghcr.io/includeamin/segmentor`, for the same architectures:

  ```sh
  docker run --rm -p 3000:3000 --read-only --cap-drop=ALL \
    -v "$PWD/vod.toml:/etc/vod/vod.toml:ro" -v "$PWD/media:/srv/vod:ro" \
    ghcr.io/includeamin/segmentor:latest
  ```

- **From source.** `cargo install --git https://github.com/includeamin/segmentor`, which needs a
  recent stable Rust toolchain and a C compiler.

[Verifying a download](docs/releasing.md#verifying-a-release) explains how to check the
attestations and the image signature.

## Supported input

Progressive and fragmented MP4, M4A, and QuickTime `.mov` files, with `moov` first or last. Nothing is decoded or re-encoded, so the codecs must already suit the protocol:

| | Supported |
| --- | --- |
| Video | H.264, HEVC (`hvc1`/`hev1`), VP9, AV1 |
| Audio | AAC-LC, HE-AAC and HE-AACv2 (explicit signaling), AC-3, E-AC-3, Opus, FLAC |
| Layout | one video track and any number of audio tracks, or audio only; edit lists of one edit, optionally after one empty edit |
| Skipped | tracks that are not audio or video (timecode, metadata, subtitles) |
| Rejected | encrypted media, samples in `moov` mixed with fragments, external data references, more than one sample description per track, other codecs (each error names what was found) |

Whether a player can decode a codec is a separate question: HEVC and the Dolby codecs need Safari or a platform decoder, for instance. See [TDD 0004](docs/technical-design/0004-broader-mp4-input-support.md) for what was verified where.

## Using it

Start the example service with:

```sh
make serve
```

The example asset is available at `http://127.0.0.1:3000/hls/sample/master.m3u8`. Configuration is loaded from `vod.example.toml`; asset IDs map to files beneath one canonical media root, and paths cannot escape that root.

Logging is configured in the same file:

```toml
[logging]
level = "info"          # trace, debug, info, warn, or error
format = "json"         # json or compact
buffer_capacity = 8192
```

Logs are written by a dedicated worker thread through a bounded, lossy queue. If the logger cannot keep up, log lines are dropped instead of blocking media requests. Request and segment timing events use `debug`, so the default `info` level records lifecycle and asset-loading events without logging every media request.

Each configured asset exposes:

```text
/hls/{asset}/master.m3u8
/hls/{asset}/video/index.m3u8
/hls/{asset}/audio-{n}/index.m3u8   (one per audio track, numbered from 1)
/hls/{asset}/{track}/init.mp4
/hls/{asset}/{track}/segments/{index}/media.m4s
/dash/{asset}/manifest.mpd
/dash/{asset}/{track}/init.mp4
/dash/{asset}/{track}/segments/{index}/media.m4s
/health   liveness
/ready    readiness (503 once shutdown begins)
/metrics  Prometheus text
```

Initialization and media responses support single and suffix byte ranges, `If-Range`, strong ETags, and immutable content-versioned URLs. Media URLs must carry the `v` query parameter the playlists emit; a missing or stale version is a `404`. Media payloads are read through a bounded backpressured stream instead of buffering the complete segment in each HTTP request.

Assets can also be resolved on demand from an external mapper service, including media held on remote HTTP origins; see the [Mapper API reference](docs/mapper-api.md) and [docs/operations.md](docs/operations.md).

CORS, shutdown behavior, concurrency limits, and metrics are configurable in the same file; see [docs/operations.md](docs/operations.md) for production guidance.

## Web player demo

`demo/index.html` is a single-file player (hls.js and dash.js, loaded from a CDN) that plays an asset over HLS or DASH and shows live server metrics parsed from `/metrics` next to it: request rate, throughput, per-route latency, errors, and resolver and cache events. It also shows player-side stats such as buffer, bandwidth, and dropped frames.

```sh
make serve   # terminal 1: the origin on :3000
make demo    # terminal 2: the player on http://127.0.0.1:8080
```

The page reads `/metrics` cross-origin, so keep `[cors]` enabled in the config, as in `vod.example.toml`.

## Packaging from the command line

The packager parses a local MP4, creates keyframe-aligned segment plans, and writes separate fragmented MP4 audio and video tracks:

```sh
cargo run -- package \
	--input tests/fixtures/h264-aac.mp4 \
	--output target/package-test
```

Regenerate the synthetic parser fixture and its FFprobe packet manifest with `make fixtures`. FFmpeg is used only for fixture generation and output validation, not by the application.

## Releases

Merges to `main` are tagged automatically with a semantic version derived from [Conventional Commits](https://www.conventionalcommits.org), so use `feat:`, `fix:`, or `type(scope)!:` in commit and pull request titles. Publish a release, with a generated changelog and a `latest` or `preview` flag, from the **Release** workflow. See [docs/releasing.md](docs/releasing.md).

## Documentation

The project handbook uses the same `mdBook` interface as the Rust Book. Install the pinned documentation tool and serve the book with live reload:

```sh
make install-doc-tools
make book-serve
```

Build the complete static site, including the `rustdoc` API reference, with `make site`. The book starts at `target/book/index.html`, and its API Reference chapter links to the generated Rust documentation under `target/book/api/`.

See [docs/README.md](docs/README.md) for the documentation layout and authoring workflow.

## Contributing and security

Contributions are welcome; start with [CONTRIBUTING.md](CONTRIBUTING.md), which covers building,
testing, design documents, and the sign-off every commit needs. Files that will not load or play
are the most useful reports there are: there is an issue form for them. Please report security
problems privately, as described in [SECURITY.md](SECURITY.md). Everyone taking part is expected to
follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Development

The repository uses stable Rust with `rustfmt` and Clippy. Run the complete local validation suite with:

```sh
make ci
```

Run `make help` to list the individual build, check, format, lint, test, and documentation targets.

`make conformance` runs the black-box HLS and DASH conformance suite, and `make bench` measures the performance budgets (see [docs/benchmarks.md](docs/benchmarks.md)).

Compile the media-pipeline fuzz target on stable with `make fuzz-check`. To run a fuzz campaign, install the separate nightly tooling. `make fuzz` copies the generated MP4 fixtures into an ignored, writable corpus before starting libFuzzer:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz --locked
make fuzz
```

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT license](LICENSE-MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
