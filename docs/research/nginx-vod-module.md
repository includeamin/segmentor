# How nginx-vod-module works

Kaltura's nginx-vod-module is an NGINX module written primarily in C. It is a useful reference architecture for on-demand packaging, but this project does not copy its source or require its runtime.

> **Licence and provenance.** nginx-vod-module is licensed under the **AGPL-3.0**, which is not compatible with this project's licence (MIT or Apache-2.0). This page describes it from its public documentation, listed under [References](#references), and from its observable behaviour. Nothing in segmentor is derived from its source, and the [contributing guide](https://github.com/includeamin/segmentor/blob/main/CONTRIBUTING.md#no-code-from-nginx-vod-module) forbids copying, translating, or adapting it. Design decisions here come from the specifications and from that behaviour.

NGINX supplies the HTTP server, event loop, request routing, file and upstream I/O, buffer chains, and response filters. The module supplies its own media pipeline:

1. Resolve a local path, remote HTTP source, or mapped media-set description.
2. Read and parse MP4 metadata, including sample tables and codec configuration.
3. Cache metadata so segment requests do not repeatedly parse the source.
4. Select tracks and calculate segment boundaries, optionally aligning them to keyframes.
5. Generate HLS, DASH, MSS, or HDS manifests with protocol-specific code.
6. Generate segment container headers with its own HLS, DASH, MP4, and MPEG-TS writers.
7. Read the selected encoded frames and send them through NGINX buffer chains.

## Does it use FFmpeg?

The normal MP4-to-HLS or MP4-to-DASH path does not invoke an `ffmpeg` command and does not require FFmpeg libraries. nginx-vod-module parses MP4 and writes output containers itself, preserving encoded samples when no filter requires decoding.

FFmpeg is an optional build dependency for features that need codec processing:

- thumbnail decoding uses `libavcodec`, with resizing through `libswscale`;
- volume-map generation decodes audio with `libavcodec`;
- playback-rate, gain, and mixing filters use libraries such as `libavcodec` and `libavfilter`;
- some audio filtering configurations also require an encoder such as `libfdk_aac`.

OpenSSL, rather than FFmpeg, supplies optional encryption and decryption support. The architectural lesson for this service is to keep packaging independent from decoding. FFmpeg is a development-time fixture and validation tool here, not a production packaging dependency.

## Performance lessons

nginx-vod-module caches MP4 metadata, keeps the packager close to source storage, uses asynchronous I/O, and expects generated media to be cached by proxies or a CDN. Those principles apply here, but their implementation must fit Axum, Tokio, immutable Rust data, and this project's explicit resource limits.

## References

- [Kaltura nginx-vod-module](https://github.com/kaltura/nginx-vod-module)
- [nginx-vod-module feature and dependency documentation](https://github.com/kaltura/nginx-vod-module#features)
- [nginx-vod-module performance recommendations](https://github.com/kaltura/nginx-vod-module#performance-recommendations)