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
