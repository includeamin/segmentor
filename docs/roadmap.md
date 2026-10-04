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
| DRM: CENC `cbcs` for DASH, `SAMPLE-AES` for HLS (Widevine, FairPlay, PlayReady, Clear Key) | Done for H.264, HEVC, and audio, including different keys per clip | [TDD 0009](technical-design/0009-common-encryption-and-drm.md) |
| Metadata caching, asynchronous I/O, CDN-friendly output | Done | [TDD 0001](technical-design/0001-on-demand-mp4-packaging-core.md), [TDD 0003](technical-design/0003-production-grade-http-api.md) |

## Phase 1: Finish DRM

DRM is the most common reason a studio-licensed catalog cannot move. These steps extend the accepted [TDD 0009](technical-design/0009-common-encryption-and-drm.md) and need no new design document.

| Step | Status | Notes |
| --- | --- | --- |
| Different keys per clip | Done | Clear pre-roll before encrypted content; `METHOD=NONE` transitions |
| HEVC encryption | Done | Slice-segment-header parser for HEVC (the `hvcC` path), checked against `FFmpeg`'s own parser on x265 and hand-built streams; same `cbcs` pattern as H.264. See [TDD 0009](technical-design/0009-common-encryption-and-drm.md#implementation-status) |
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

## Performance: faster than nginx-vod-module everywhere

The goal is for segmentor to beat nginx-vod-module on every measurement of the [head-to-head comparison](benchmarks.md#segmentor-vs-nginx-vod-module), not only on segment throughput. Each step is measured with `make bench-compare` before and after, and gives back nothing elsewhere.

Where it stands (2026-10-03, 60-minute asset):

| Measurement | segmentor | nginx-vod-module | Gap |
| --- | ---: | ---: | --- |
| First segment from a fresh process | 8.5 ms | 19.4 ms | segmentor 2.3× ahead (was 245 ms against 69 ms) |
| First master playlist from a fresh process | 6.6 ms | 4.6 ms | nginx-vod-module 1.4× faster, by deferring its parse to the media playlist |
| Playlist requests/s on 1 core, response cache on | 17,603 | 12,988 | segmentor 1.36× ahead |
| A viewer's playlists per second on 1 core, response cache on | 8,554 muxed, 5,868 separate | 6,494 | segmentor 1.32× ahead when muxed; behind with audio as its own rendition |
| Segment throughput, 1 core | 447 MiB/s | 197 MiB/s | segmentor 2.3× ahead |

**How.** Work largest gap first: cold start, then playlists, then segments.

- **Cold start:** do less before the first answer and do the rest faster. Render playlists only when first asked for. Hash less, and with a faster hash. Keep a compact index, expanded one segment at a time, instead of a 32-byte record per sample. Keep parsed indexes on disk across restarts.
- **Playlists:** send fewer bytes and fewer requests. Compress playlists once and keep the compressed forms. Offer audio muxed into the video playlist, so a viewer needs two playlists instead of three.
- **Segments:** remove what is left of the per-request overhead. Build small fragment headers inline instead of on the blocking pool. Send coalesced reads as vectored writes instead of copying. Evaluate `io_uring`, and offer muxed audio and video as an option.
- **Measure, don't guess:** a CPU profiler, `make bench-compare` as a recorded regression check, and profile-guided optimization of release builds.

| Step | Status | Notes |
| --- | --- | --- |
| Head-to-head benchmark | Done for local clear content | `make bench-compare`, with nginx-vod-module measured with and without its response cache; extend to remote sources, encryption, and many assets |
| Coalesced segment reads | Done | One read per nearby group of samples instead of per sample: 52 to 692 MiB/s |
| Cold-start profiling | Done | Hashing dominated: `moov` is hashed once, with BLAKE3, and the mutation check compares bytes. See [benchmarks](benchmarks.md#what-it-found-a-slow-first-request) |
| Lazy playlist rendering | Done | The master is rendered on load and every other playlist on its first request |
| Direct sample-table parsing | Done | The tables are read from the `moov` bytes and written straight into the sample list, with tracks expanded in parallel |
| Header-only master playlist | Decide first | The compact index made a full load about 4 ms in process; most of the remaining 6.7 ms is fresh-process cost. A two-stage load (master first, index after) would save perhaps 2 ms more, at the cost of a second load path whose bandwidth figures must match the full one |
| Compact sample index | Done | [TDD 0010](technical-design/0010-compact-sample-index.md). Load 8.5 to 4 ms in process, index 8.9 to 4.4 MB on the 60-minute asset, output byte-identical |
| Persistent index cache | To do | Indexes on disk keyed by the `moov` hash, so restarts and deploys are not cold |
| Version in the path, short segment URIs | Dropped | Compression made it moot: the repeated `?v=` compresses to almost nothing, and it would change public URLs |
| Compressed playlists | Done | brotli or gzip by `Accept-Encoding`, each made once on first request: 34 KB to 857 bytes for a one-hour media playlist |
| Precomputed response headers | Not needed for now | Header building does not show at 16,900 requests/s per core; revisit with a profile |
| Interleaved reads for muxed segments | Done | Each file region of a muxed segment is read once instead of once per track: 2-core throughput 488 to about 800 MiB/s |
| Inline fragment headers | To do | Below a sample-count threshold, skip the blocking-pool hop |
| Vectored writes for coalesced reads | To do | No copy into a joined buffer |
| `io_uring` for local reads | Evaluate | Keep only if the comparison shows a gain |
| Muxed audio and video | Done for clear single-file assets, on by default | `packaging.hls_mux_audio`, [TDD 0011](technical-design/0011-muxed-hls-audio.md): half the segment requests and one fewer playlist per viewer. Encrypted, adaptive, and sequence assets still serve audio separately |
| Profile-guided optimization | To do | Release builds trained on the benchmark workload |
| Comparison under remote sources, encryption, and many assets | To do | The places where the two read and cache most differently |

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
2. **Performance** and **Phase 4**, done in parallel: the vendor validators, browser suite, and reference benchmarks give the evidence a production cutover needs, whatever features are added.
3. **Phase 2**: SRT input, segmented subtitles, and MP3 cover most remaining catalogs.
4. **Phase 5**: the migration path, once the features a given deployment uses are covered.
5. **Phase 3**: only the items a real deployment needs; simulated live is the most likely.
