# Protocol conformance

The service's HLS and DASH output is checked by an automated black-box suite, and the remaining vendor and browser checks are listed with how to run them.

## The automated suite

`tests/conformance.rs` starts the real binary against five fixtures and audits what a player would fetch. Run it with `make conformance` (it also runs under `make test` and in CI).

Fixtures: the main H.264/AAC file, the same with `moov` after `mdat`, video only, 44.1 kHz stereo audio, and variable frame timing.

### HLS (RFC 8216)

- Playlist starts with `#EXTM3U`; the protocol version is at least 6, which fMP4 with `EXT-X-MAP` requires.
- The variant has `BANDWIDTH` not below `AVERAGE-BANDWIDTH`, a codec string, and a resolution.
- An `AUDIO` group reference on the variant matches exactly one `EXT-X-MEDIA` rendition, and a rendition without a reference is rejected.
- Media playlists are `VOD`, `INDEPENDENT-SEGMENTS`, end with `ENDLIST`, and have an `EXT-X-MAP`.
- Every `EXTINF` rounded to whole seconds does not exceed `EXT-X-TARGETDURATION`.
- Every URI in every playlist resolves and returns `200`.

### DASH (ISO/IEC 23009-1)

- The manifest has the MPD namespace, `type="static"`, a DASH profile, and valid `mediaPresentationDuration` and `minBufferTime`.
- Each `Representation` has bandwidth and codecs and a `SegmentTemplate` with a `SegmentTimeline`.
- `$RepresentationID$` and `$Number$` substitutions resolve to real objects.
- The timeline does not exceed the presentation duration, and the longest track equals it.

### Media bytes (ISO BMFF, CMAF-style layout)

- Init segments have `ftyp`, `moov`, exactly one `trak`, `mvex/trex`, and empty sample tables.
- Media segments are `[styp] moof mdat`, every box size tiles its parent exactly, and `tfhd` uses default-base-is-moof.
- The `trun` data offset lands exactly on the first payload byte, and the sample sizes sum to the `mdat` payload.
- `mfhd` sequence numbers run 1, 2, 3 ...; `tfdt` values are contiguous, each starting where the previous segment ended.
- Video segments begin with a sync sample.
- The duration declared in the playlist or timeline matches the sum of sample durations in the fragment, within one millisecond of rounding.
- **HLS and DASH serve byte-identical init and media segments** for every track.

### Transport

Byte ranges (`bytes=a-b`, suffix `bytes=-n`), `Accept-Ranges`, `ETag`, `If-None-Match`, stale `If-Range`, and `immutable` caching on media objects.

### Decoding

When FFprobe and FFmpeg are installed, each track is reassembled (init plus segments), decoded with `ffmpeg -v error` with no errors, and its packet count is compared with the source file's.

The suite was mutation-checked: making the fragment sequence numbers start at 2 fails it immediately.

## What is not covered

The suite verifies structural rules the code can be held to. It is not a certification.

| Check | Status | How to run it |
| --- | --- | --- |
| Apple HLS validation (`mediastreamvalidator`, `hlsreport`) | Not run | Requires macOS and Apple's HTTP Live Streaming Tools. Point the validator at `http://<host>/hls/<asset>/master.m3u8` before a release |
| DASH-IF conformance (`DASH-IF-Conformance` tool or web validator) | Not run | Needs Java or the hosted validator. Run it on a served MPD before a release |
| CMAF (ISO/IEC 23000-19) profile conformance | Not claimed | The layout is CMAF-style but no CMAF validator has been run. Do not claim conformance |
| Browser playback with hls.js and dash.js (start, seek, play) | Not implemented | Needs Node, `npm`, and a browser driver such as Playwright. Neither `npm` nor a driver was available where this suite was written |
| Device and player matrix (Safari, tvOS, Android ExoPlayer, smart TVs) | Not covered | Manual |

Until the vendor validators and browser tests are run and their output recorded here, treat the output as structurally sound and cross-checked but not certified.
