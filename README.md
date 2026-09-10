# vod-module-rs

A video-on-demand application built with Rust.

## Development

The repository uses stable Rust with `rustfmt` and Clippy. Run the complete local validation suite with:

```sh
make ci
```

Run `make help` to list the individual build, check, format, lint, test, and documentation targets.

## Packaging prototype

The first implementation slice parses a local H.264/AAC MP4, creates keyframe-aligned segment plans, and writes separate fragmented MP4 audio and video tracks:

```sh
cargo run -- package \
	--input tests/fixtures/h264-aac.mp4 \
	--output target/package-test
```

Regenerate the synthetic parser fixture and its FFprobe packet manifest with `make fixtures`. FFmpeg is used only for fixture generation and output validation, not by the application.

## HLS service

Start the example service with:

```sh
make serve
```

The example asset is available at `http://127.0.0.1:8080/hls/sample/master.m3u8`. Configuration is loaded from `vod.example.toml`; asset IDs map to files beneath one canonical media root, and paths cannot escape that root.

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
/hls/{asset}/audio/index.m3u8
/hls/{asset}/{track}/init.mp4
/hls/{asset}/{track}/segments/{index}/media.m4s
```

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
