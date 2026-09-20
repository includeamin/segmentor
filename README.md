# segmentor

A video-on-demand application built with Rust.

> **Work in progress.** This project is under active development. APIs, configuration, and behavior may change without notice, and it is not yet ready for production use.

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

## Packaging prototype

The packager parses a local MP4, creates keyframe-aligned segment plans, and writes separate fragmented MP4 audio and video tracks:

```sh
cargo run -- package \
	--input tests/fixtures/h264-aac.mp4 \
	--output target/package-test
```

Regenerate the synthetic parser fixture and its FFprobe packet manifest with `make fixtures`. FFmpeg is used only for fixture generation and output validation, not by the application.

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

## HLS service

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

## License

Licensed under the [MIT License](LICENSE).
