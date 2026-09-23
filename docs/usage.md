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
/hls/{asset}/video/index.m3u8       (a plain asset)
/hls/{asset}/video-{id}/index.m3u8  (one per video rendition of an adaptive asset)
/hls/{asset}/audio-{n}/index.m3u8   (the shared audio group, numbered from 1)
/hls/{asset}/video/iframes.m3u8     (I-frame playlist, when there is video)
/hls/{asset}/video/iframes/{n}/media.m4s   (one keyframe as its own fragment)
/hls/{asset}/{track}/init.mp4
/hls/{asset}/{track}/segments/{index}/media.m4s
/hls/{asset}/subtitles/{language}/index.m3u8   (when the mapper lists subtitles)
/hls/{asset}/subtitles/{language}/sub.vtt
/dash/{asset}/manifest.mpd
/dash/{asset}/subtitles/{language}/sub.vtt
/dash/{asset}/{track}/init.mp4
/dash/{asset}/{track}/segments/{index}/media.m4s
/health         liveness
/ready          readiness (503 once shutdown begins)
/metrics        Prometheus text
/admin/status   JSON: resolver health and the loaded-asset cache, checked live (see below)
```

Initialization and media responses support single and suffix byte ranges, `If-Range`, strong ETags, and immutable content-versioned URLs. Media URLs must carry the `v` query parameter the playlists emit; a missing or stale version is a `404`. Media payloads are streamed through a bounded, backpressured reader instead of being buffered per request.

Configuration, logging, limits, CORS, and shutdown are described in [Operating the origin](operations.md).

## Web player demo

`demo/index.html` is a single-file player (hls.js and dash.js, loaded from a CDN) that plays an asset over HLS or DASH and shows live server metrics parsed from `/metrics` next to it: request rate, throughput, per-route latency, errors, and resolver and cache events, plus player-side stats such as buffer, bandwidth, and dropped frames. Besides typing an asset ID, a "Try:" row under the input lists assets you can click straight into: the static catalog's, and anything already played, from `/admin/status`.

```sh
make serve   # terminal 1: the origin on :3000
make demo    # terminal 2: the player on http://127.0.0.1:8080
```

The page reads `/metrics` cross-origin, so keep `[cors]` enabled, as in `vod.example.toml`.

## Control panel

`admin/index.html` is a second single-file page, separate from the player demo, for operating a running instance: it polls `/admin/status` for the resolver's live connection state and every asset currently in the loaded-asset cache (version, size, tracks, duration), and `/metrics` for the same request and throughput charts the player demo shows. It also embeds the same HLS/DASH player, with a "Known asset" dropdown next to the Asset field listing everything the status view names, so you can pick a playable asset without leaving the page or typing its ID — the field itself still takes any name, known or not.

```sh
make serve   # terminal 1: the origin on :3000
make admin   # terminal 2: the panel on http://127.0.0.1:8081
```

`/admin/status` has no authentication of its own, the same as `/metrics`; see [Operating the origin](operations.md) for what to put in front of it before this page is reachable by anyone but you.

To see the panel with a mapper resolver instead of the static catalog, `docker compose -f docker-compose.dev.yml up --build` runs segmentor, an [example mapper](https://github.com/includeamin/segmentor/tree/main/examples/mapper), and both web pages together; see [Try it](mapper-api.md#try-it).

## Packaging from the command line

`package` parses a local MP4, plans keyframe-aligned segments, and writes separate fragmented MP4 audio and video tracks:

```sh
cargo run -- package --input tests/fixtures/h264-aac.mp4 --output target/package-test
```
