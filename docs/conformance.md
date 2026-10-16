# Protocol conformance

The service's HLS and DASH output is checked by an automated black-box suite, and the remaining vendor and browser checks are listed with how to run them.

## The automated suite

`tests/conformance.rs` starts the real binary against every fixture below, and against assets served through a mapper, and audits what a player would fetch. Run it with `make conformance` (it also runs under `make test` and in CI).

Fixtures: the main H.264/AAC file, the same with `moov` after `mdat`, video only, 44.1 kHz stereo audio, and variable frame timing; FFmpeg's default edit lists and a delayed audio track; two audio tracks; non-square pixels with tagged colour; a QuickTime file; HEVC, VP9, AV1; AC-3, E-AC-3, Opus, and FLAC audio; and audio-only files with one and two tracks. Every reassembled track is decoded by FFmpeg without errors and its packet count compared with the source.

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

- Init segments have `ftyp`, `moov`, exactly one `trak`, `mvex/trex`, and empty sample tables, for the separate renditions these checks run against.
- Media segments are `[styp] moof mdat`, every box size tiles its parent exactly, and `tfhd` uses default-base-is-moof.
- The `trun` data offset lands exactly on the first payload byte, and the sample sizes sum to the `mdat` payload.
- `mfhd` sequence numbers run 1, 2, 3 ...; `tfdt` values are contiguous, each starting where the previous segment ended.
- Video segments begin with a sync sample.
- The duration declared in the playlist or timeline matches the sum of sample durations in the fragment, within one millisecond of rounding.
- **HLS and DASH serve byte-identical init and media segments** for every track.
- **Muxed HLS** ([TDD 0011](technical-design/0011-muxed-hls-audio.md), the default): for every fixture with video and audio, the master points at the muxed stream, its init segment has both `trak` boxes, and each fragment's `mdat` is exactly the separate video fragment's payload followed by the audio fragment's, with a `traf` for each (audio only while it lasts). FFmpeg decodes each muxed presentation.

### Transport

Byte ranges (`bytes=a-b`, suffix `bytes=-n`), `Accept-Ranges`, `ETag`, `If-None-Match`, stale `If-Range`, and `immutable` caching on media objects.

### Decoding

When FFprobe and FFmpeg are installed, each track is reassembled (init plus segments), decoded with `ffmpeg -v error` with no errors, and its packet count is compared with the source file's.

### Assets served through a mapper

The fixtures above come from a static catalog of single files, which cannot express renditions or clips. A second set of tests starts `examples/mapper` on a free port, with `tests/fixtures/harness-catalog.json`, and a server that resolves its assets from it, so these are audited out of process too:

| Asset | What it is |
| --- | --- |
| `ladder` | Two video renditions and an audio rendition ([TDD 0006](technical-design/0006-trick-play-subtitles-and-renditions.md)) |
| `preroll` | A sequence of two clips of different files ([TDD 0008](technical-design/0008-clipping-and-concatenation.md)) |
| `grid` | Renditions that are sequences ([TDD 0016](technical-design/0016-adaptive-clips.md)), each cut at 1000 ms, which is before the second keyframe because a video clip starts on the keyframe at or before its window |
| `trimmed` | A ladder with one clip per rendition |

Both protocols are followed clip by clip: each `EXT-X-MAP` in a playlist and each `Period` in a manifest is a clip. On top of the per-track rules above:

- segment numbers continue across clips, every clip has its own init segment, and a presentation of one clip has a plain one;
- a discontinuity precedes every clip but the first, and nothing else;
- each clip starts where the one before ended, within 40 ms;
- the video renditions have the same clips, the same number of segments in each, and start each clip together, within 40 ms;
- each audio track has the video's clips, starts each within 50 ms of the video, and is within 100 ms of its length, which is what shows an audio clip cut from a different instant of its source than the video's;
- every period has the same representations, and a period's `presentationTimeOffset` is where its first fragment starts (an audio clip may start up to 50 ms later, at its first frame);
- HLS and DASH serve byte-identical init and media segments for every clip of every track;
- FFmpeg decodes every clip of every track on its own, since it cannot judge a sequence as one stream.

### Mutation checks

Each audit was checked by breaking the server and confirming that the suite fails, then restoring it (2026-10-16):

| Mutation | Caught by |
| --- | --- |
| The fragment sequence numbers start at 2 | The fixture audit, the mapper-driven audit, and both decode tests |
| Audio-only clips are cut from the mapper's own window instead of the video's span (the bug found in the review of TDD 0016) | `a_grid_cut_mid_gop_conforms` and the mapper decode test |
| The second DASH period lacks a representation | `a_grid_cut_mid_gop_conforms` |

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
