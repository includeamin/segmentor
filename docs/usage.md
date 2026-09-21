# Using segmentor

## Run the example

```sh
make serve
```

serves `tests/fixtures/h264-aac.mp4` as the asset `sample`, using `vod.example.toml`. Asset IDs map to files beneath one canonical media root, and paths cannot escape it. Assets can also be resolved on demand from a [mapper](mapper-api.md), including media on remote HTTP origins.

## Endpoints

Each asset exposes:

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

Initialization and media responses support single and suffix byte ranges, `If-Range`, strong ETags, and immutable content-versioned URLs. Media URLs must carry the `v` query parameter the playlists emit; a missing or stale version is a `404`. Media payloads are streamed through a bounded, backpressured reader instead of being buffered per request.

Configuration, logging, limits, CORS, and shutdown are described in [Operating the origin](operations.md).

## Web player demo

`demo/index.html` is a single-file player (hls.js and dash.js, loaded from a CDN) that plays an asset over HLS or DASH and shows live server metrics parsed from `/metrics` next to it: request rate, throughput, per-route latency, errors, and resolver and cache events, plus player-side stats such as buffer, bandwidth, and dropped frames.

```sh
make serve   # terminal 1: the origin on :3000
make demo    # terminal 2: the player on http://127.0.0.1:8080
```

The page reads `/metrics` cross-origin, so keep `[cors]` enabled, as in `vod.example.toml`.

## Packaging from the command line

`package` parses a local MP4, plans keyframe-aligned segments, and writes separate fragmented MP4 audio and video tracks:

```sh
cargo run -- package --input tests/fixtures/h264-aac.mp4 --output target/package-test
```
