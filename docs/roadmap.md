# Roadmap

This page lists everything segmentor still needs before it can replace [nginx-vod-module](research/nginx-vod-module.md) in production. Each step is grouped into a phase, with its status and the design it belongs to.

- **Feature parity** means segmentor does everything nginx-vod-module does that someone actually relies on. It does not mean copying nginx-vod-module's configuration syntax or URL scheme. Migrating an existing deployment is a separate step (Phase 5).
- **Out of scope by design** lists features that conflict with segmentor's architecture. They would need an ADR before any code.

The baseline is nginx-vod-module's [published feature list](https://github.com/kaltura/nginx-vod-module#features), compared with the status lines of TDDs 0001 to 0009.

## Where segmentor already matches

| nginx-vod-module feature | segmentor | Design |
| --- | --- | --- |
| On-the-fly repackaging of MP4 to HLS and DASH | Done | [TDD 0001](technical-design/0001-on-demand-mp4-packaging-core.md) |
| Local, remote (HTTP range), and mapped working modes | Done; mapped mode uses segmentor's own mapper API | [TDD 0002](technical-design/0002-asset-map-interface.md) |
| Adaptive bitrate, alternative audio renditions | Done | [TDD 0006](technical-design/0006-trick-play-subtitles-and-renditions.md) |
| HLS I-frame playlists | Done, for clear assets and encrypted ones | [TDD 0006](technical-design/0006-trick-play-subtitles-and-renditions.md) |
| WebVTT subtitles (HLS and DASH) | Done, as sidecar files | [TDD 0006](technical-design/0006-trick-play-subtitles-and-renditions.md) |
| H.264, HEVC, VP9, AV1 video; AAC, AC-3, E-AC-3, Opus, FLAC audio | Done (clear) | [TDD 0004](technical-design/0004-broader-mp4-input-support.md) |
| Audio-only and video-only files | Done | [TDD 0004](technical-design/0004-broader-mp4-input-support.md) |
| Fragmented MP4 input | Done | [TDD 0005](technical-design/0005-fragmented-mp4-input.md) |
| Source clipping; playing several files back to back | Done | [TDD 0008](technical-design/0008-clipping-and-concatenation.md) |
| Variable segment lengths | Done, inherent to keyframe-aligned planning | [TDD 0001](technical-design/0001-on-demand-mp4-packaging-core.md) |
| DRM: CENC `cbcs` for DASH, `SAMPLE-AES` for HLS (Widevine, FairPlay, PlayReady, Clear Key) | Done for H.264 and audio, including different keys per clip | [TDD 0009](technical-design/0009-common-encryption-and-drm.md) |
| Metadata caching, asynchronous I/O, CDN-friendly output | Done | [TDD 0001](technical-design/0001-on-demand-mp4-packaging-core.md), [TDD 0003](technical-design/0003-production-grade-http-api.md) |

## Phase 1: Finish DRM

DRM is the most common reason a studio-licensed catalog cannot move. These steps extend the accepted [TDD 0009](technical-design/0009-common-encryption-and-drm.md) and need no new design document.

| Step | Status | Notes |
| --- | --- | --- |
| Different keys per clip | Done | Clear pre-roll before encrypted content; `METHOD=NONE` transitions |
| HEVC encryption | To do | Slice-header parser for HEVC (the `hvcC` path); same `cbcs` pattern as H.264 |
| Whole-segment HLS AES-128 | To do | nginx-vod-module's "HLS AES-128": simple protection without a DRM vendor. Whole-fragment AES-128-CBC with a key URI, independent of `cbcs` |
| Key rotation within an asset | To do | Key periods over the timeline. HLS: new `EXT-X-KEY` at period boundaries. DASH: per-Period or `pssh` in fragments |
| Decrypting CENC-encrypted source files | To do | nginx-vod-module repackages already-encrypted MP4s. Needs `encv`/`enca` parsing and a source-key field in the mapper answer |
| AV1 and VP9 encryption | To do | OBU-aware and superframe-aware subsample rules |
| Opus and FLAC encryption | To do | Whole-sample protection, as for AAC |
| `cenc` (AES-CTR) scheme | Decide first | Only needed for players that cannot do `cbcs`. Confirm a real target device needs it before building |
| SPEKE v2 key exchange | To do, mapper side | segmentor never talks to a licence server, so this is a reference mapper adapter and guide, not server code |
| Widevine, FairPlay, PlayReady playback on real devices | To do | Manual release check with an operator's DRM vendor account; only Clear Key is verified in browsers today |

## Phase 2: Captions and codecs

| Step | Status | Notes |
| --- | --- | --- |
| SRT input converted to WebVTT | To do | The most common format in source libraries ([TDD 0006](technical-design/0006-trick-play-subtitles-and-renditions.md) open question) |
| TTML/DFXP input | To do | Needs an XML parser; keep styling out of scope as nginx-vod-module does |
| CAP (Cheetah) input | Decide first | Rare; build only for a known catalog that uses it |
| Segmented subtitles | To do | Segmented WebVTT for HLS, and WebVTT or SMPTE-TT segments for DASH, instead of one sidecar file |
| Text tracks inside the MP4 (`tx3g`, `wvtt`) | To do | Today only sidecar files are served |
| Subtitles together with clips | To do | Rejected today because cues would drift by up to a GOP ([TDD 0008](technical-design/0008-clipping-and-concatenation.md)) |
| MP3 audio | To do | HLS only in nginx-vod-module; today rejected inside `mp4a` |
| DTS audio | Decide first | HLS only in nginx-vod-module |
| Vorbis audio | Decide first | DASH only, and mostly a WebM concern |
| Muxing audio and video from separate files into one stream | To do | nginx-vod-module serves a separate audio file as part of the video's stream, without client support for rendition groups |
| Track selection for files with several audio or video tracks | To do | Let the mapper choose which tracks of a file to serve |

## Phase 3: Delivery modes and protocols

These change what segmentor is, so each starts with a new TDD.

| Step | Status | Notes |
| --- | --- | --- |
| Simulated live from VOD files | New TDD | A live playlist, a sliding window, and a wall-clock timeline over the clip sequences that already exist. [TDD 0008](technical-design/0008-clipping-and-concatenation.md) defers it explicitly |
| Fallback on file not found | New TDD | nginx-vod-module retries another datacenter on a miss. In segmentor this is a mapper concern (a second location) or an origin failover list |
| Ad markers (`EXT-X-DATERANGE`, DASH events) | New TDD | Deferred by [TDD 0008](technical-design/0008-clipping-and-concatenation.md), together with linear channels |
| Microsoft Smooth Streaming (MSS) | Decide first | A third manifest and fragment layout, plus PlayReady for MSS. Only worth it for a known Xbox or legacy smart-TV audience |
| Adobe HDS | Not recommended | Requires Flash, which no current player supports |
| MPEG-TS segments for HLS | Decide first | nginx-vod-module serves TS; segmentor serves fMP4 ([ADR 0001](adr/0001-use-fragmented-mp4-for-media-segments.md)). Needed only for very old HLS players |
| Clipped MP4 for progressive download | Decide first | A whole-file MP4 response for a window, for players that cannot do HLS or DASH |

## Phase 4: Production readiness

| Step | Status | Notes |
| --- | --- | --- |
| Viewer authorization (signed tokens, optional live check) | Draft | [TDD 0007](technical-design/0007-viewer-authorization.md). nginx-vod-module relies on NGINX modules for this |
| Apple Media Stream Validator on served HLS | To do | Release-time check ([conformance](conformance.md)) |
| DASH-IF conformance tool on served MPDs | To do | Release-time check |
| Browser playback suite (hls.js, dash.js, Safari native) | To do | Start, seek, and play, across the codecs and DRM modes above |
| Benchmarks on the reference host | To do | Budgets are met on a laptop ([benchmarks](benchmarks.md)); record them on the documented four-core host |
| Load test against a production-like catalog | To do | Cold storage, many assets, remote origins, and CDN miss traffic, not one warm file |
| Mapper bearer-token rotation without restart | To do | Re-read the token file periodically |
| Tests against a real HTTPS mapper and origin | To do | Today only in-process mocks |

## Phase 5: Migration from nginx-vod-module

| Step | Status | Notes |
| --- | --- | --- |
| Mapping-JSON compatibility layer | New TDD | An adapter that turns nginx-vod-module's mapped-mode JSON into a segmentor mapper answer, so existing mapping services keep working. [TDD 0008](technical-design/0008-clipping-and-concatenation.md) defers "vod-module mapping compatibility" |
| URL compatibility, or a redirect layer | Decide first | nginx-vod-module URLs differ from segmentor's. Players that load URLs from the mapper need nothing; hard-coded URLs need a rewrite at the proxy |
| Migration guide | To do | Feature mapping, configuration translation, and a shadow-traffic comparison procedure |
| Shadow comparison in production | To do | Run both behind the same CDN on a sample of traffic, and compare manifests, segment decodes, and error rates before cutover |

## Out of scope by design

segmentor never decodes media. The following nginx-vod-module features need decoding (it uses FFmpeg libraries for them), so they would need an ADR that changes that principle:

- thumbnail capture and resizing;
- volume maps;
- playback-rate change, gain, and audio mixing filters.

If one of them is needed, the usual approach is a separate service next to segmentor (for example a thumbnail service reading the same storage), keeping the packaging path decode-free.

## order

1. **Phase 1** (finish DRM): it is the most common blocker for premium catalogs, and it needs no new designs.
2. **Phase 4**, done in parallel: the vendor validators, browser suite, and reference benchmarks give the evidence a production cutover needs, whatever features are added.
3. **Phase 2**: SRT input, segmented subtitles, and MP3 cover most remaining catalogs.
4. **Phase 5**: the migration path, once the features a given deployment uses are covered.
5. **Phase 3**: only the items a real deployment needs; simulated live is the most likely.
