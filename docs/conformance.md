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
| `single` | One file, resolved through the mapper |
| `ladder` | Two video renditions and two audio-only renditions, one per language ([TDD 0006](technical-design/0006-trick-play-subtitles-and-renditions.md)) |
| `preroll` | A sequence of two clips of different files ([TDD 0008](technical-design/0008-clipping-and-concatenation.md)) |
| `grid` | Renditions that are sequences ([TDD 0016](technical-design/0016-adaptive-clips.md)), each cut at 1000 ms, which is before the second keyframe because a video clip starts on the keyframe at or before its window |
| `trimmed` | A ladder with one clip per rendition |

Both protocols are followed clip by clip: each `EXT-X-MAP` in a playlist and each `Period` in a manifest is a clip. On top of the per-track rules above:

- segment numbers continue across clips, every clip has its own init segment, and a presentation of one clip has a plain one;
- a discontinuity precedes every clip but the first, and nowhere else;
- each clip starts where the one before ended, to within a sample;
- the video renditions are cut alike: the same clips, the same segments starting at the same instants, and each clip starting and ending together, each to within a sample;
- a video clip holds at least the window it was asked for (it may start earlier, never later or shorter);
- each audio track has the video's clips, starts each within one audio frame of the video, and is the video's length to within a video sample and an audio frame, which is what shows an audio clip cut from a different instant of its source than the video's. Only the last clip may run longer, by up to 100 ms of trailing audio;
- every period has the same representations, and a period's `presentationTimeOffset` is where its first fragment starts, or up to one sample before an audio clip's first frame;
- HLS and DASH serve byte-identical init and media segments for every clip of every track;
- FFmpeg decodes every clip of every track on its own, since it cannot judge a sequence as one stream.

The tolerances are not fixed numbers: they come from the first sample duration of the clips being compared, so a coarser frame rate loosens them and a finer one tightens them.

Every rule between tracks and clips is a plain function over a clip's shape in milliseconds, and each has a synthetic test with a known-bad input that must be refused, so a rule cannot go vacuous without a test failing. A playlist's clip structure has the same.

### Known deviation: a sequence's duration

A sequence states the end of its video as `mediaPresentationDuration`, and its last clip keeps the audio that runs past the video ([TDD 0008](technical-design/0008-clipping-and-concatenation.md)), so an audio track can end after the stated duration: by 21 ms in `grid` and 66 ms in `preroll` today. A single file states its longest track instead and is held to 2 ms. The mapper-driven audit allows 100 ms, which is a little over the worst case seen; it is there to catch a larger overrun, not to excuse this one. Whether the duration should cover the longest track, as for a single file, is open.

### Mutation checks

Each audit was checked by breaking the server and confirming that the suite fails, then restoring it (2026-10-16). Some faults are also refused by the server's own validation, which makes the asset fail to load; those are marked, because in that case the audit is not what caught them.

| Mutation | Caught by |
| --- | --- |
| The fragment sequence numbers start at 2 | The fixture audit and the mapper-driven audit, and both decode tests |
| Audio-only clips are cut from the mapper's own window instead of the video's span, and the server's own audio length check is removed (the bug found in the review of TDD 0016, as it first shipped) | The clip-continuity rule in `a_grid_cut_mid_gop_conforms`: "clip 0 ends at 1002 ms and clip 1 starts at 933 ms". The decode test does **not** catch it. With the server's check left in, the server refuses the asset |
| The second DASH period lacks a representation | The period-representation rule in `a_grid_cut_mid_gop_conforms` |
| No discontinuity before the second clip | The discontinuity rule, on `grid` and `preroll` |
| The DASH `startNumber` is one too high in later periods | The media requests of the DASH audit (the segment does not exist) |
| Audio-only clips are placed 100 ms late on the timeline | The server's own length check refuses the asset. The audit's audio-start rule is covered by its synthetic test only |
| The grid's second clip asks for a window from 2100 ms, past the next keyframe | The window rule: "clip 1 is 1000 ms long, but its window holds at least 2000 ms" |

### Browser playback

The same assets are played in a real browser. `tests/browser` is a Node project (Playwright, with hls.js and dash.js installed from npm at pinned versions, never from a CDN) that starts the example mapper, the server and a small player page, and drives them. Run it with `make browser-test`; it needs Node 22 or later, `python3`, and **Google Chrome** (`npx playwright install chrome` on a runner), because Playwright's bundled Chromium has no H.264 or AAC. In CI it is the `Browser tests` workflow, which is not a required check yet.

Measured on 2026-10-16 with hls.js 1.7.3, dash.js 5.2.1 and Chrome 155. For each of `single`, `ladder`, `preroll`, `grid` and `trimmed`, under each player:

- **starts**: playback passes half a second with no player error;
- **ends for the length the manifest states**: `ended` fires, and the duration is within 0.25 s of `mediaPresentationDuration`;
- **plays all of it**: what was played covers the presentation, less what is explained (the video starts 67 ms in, the last 60 ms is not waited for, and each join between clips may leave one reorder delay of 67 ms);
- **seeks**: to 60% and to half a second before the end, each made while paused and then resumed, since the assets last only a few seconds.

On `ladder` and `grid`, **quality switching** forces each rendition in turn, up then down and down then up, with the player's own adaptive switching turned off, and each switch must show as a change in picture size. A player replaces what is buffered after the fragment it is playing, so a switch shows at the next segment boundary; each waits for the one before. The first switch on `grid` lands at the join between its two clips.

A test that is known to fail is marked as an expected failure in the spec, with the reason, so it fails the run the day it starts passing and the entry has to go. Tests that fail only some of the time are tagged `@intermittent` and left out of the default run; `INTERMITTENT=1 npx playwright test` includes them. Retrying them would only hide a player that sometimes does not finish.

### Found by the browser suite

| Finding | Evidence | In the suite |
| --- | --- | --- |
| **The DASH manifest of a ladder puts each video rendition in an `AdaptationSet` of its own.** In DASH the switchable steps of one piece of content belong in one adaptation set; separate ones are alternative tracks. So dash.js lists a single rendition and can never adapt between them. This affects every ladder, not only clips | The `ladder` MPD has two video adaptation sets of one representation each. dash.js `getRepresentationsByType('video')` returns 1 | The four dash.js quality specs are expected failures |
| **dash.js skips about 0.3 s at the join between clips** | `preroll` presents 141 of 150 frames and `grid` 112 of 118; `played` ranges `[0.07, 2.73]` then `[3.07, 5.01]`. A sequence of video-only files does the same; a sequence of audio-only files plays perfectly | `dash plays preroll` and `dash plays grid`, "plays all of it", are expected failures |
| **hls.js never fires `ended` on a sequence of whole files** | It stops 35 ms short of the end with a `bufferStalledError`, no fatal error, and its main stream controller stays `IDLE` while the audio one ends. The video buffer has a 66 ms hole at the join. The same file played twice does the same, and the reported duration is 6.13 s for 6.0 s | `hls plays preroll`, "ends", is an expected failure |
| **hls.js on the adaptive-clips `grid` is unreliable** | It ends in most isolated runs, but in the full suite twice in a row it did not, and it does not end whenever a quality switch is made at the join: the duration comes out as 4.1 s instead of 4.0 s, about 0.3 s around the join is skipped, and it stalls short of the end | `hls plays grid`, "ends" and "plays all of it", are `@intermittent` |

What was ruled out: the audio (audio-only sequences are fine), a change of resolution (the same file twice fails), a clip that starts mid-GOP, and hls.js's tolerance for holes (widening `maxBufferHole` to 0.3 s changed nothing). What was not determined is the cause. The one pattern is that every problem is at a join between clips, in the video, in both protocols. A reasonable guess is the timeline at the join, where the last frames of a clip are presented up to 67 ms after the next clip is declared to start ([TDD 0008](technical-design/0008-clipping-and-concatenation.md), "Timeline"), but nothing here proves it. Telling a defect in the output from a quirk of these two players needs a third player (Shaka Player) and a decision on that design.

Each layer is mutation-checked like the one before (2026-10-16): the server is broken, the suite is run, and the server is restored.

| Mutation | Caught by the browser suite |
| --- | --- |
| The init segment of every clip after the first answers 404 | 11 specs, under both players, on `preroll` and `grid` |
| The 480p rendition cannot be reached after the first clip | The 4 hls.js specs on `grid` that start on, or switch to, 480p |
| No `EXT-X-DISCONTINUITY` before the second clip | No spec fails, but `hls plays preroll`, "ends", an expected failure, **passes**, so the run reports it: hls.js does reach the end of the sequence without the discontinuity. That points the end-of-stream problem above at how hls.js handles it. Leaving the tag out is not a fix: clips can differ in encoding |
| Audio-only clips are cut from the mapper's own window, with the server's own length check removed (the bug found in the review of TDD 0016) | **Nothing.** A player starts, seeks and plays a stream whose audio is out of step with its video. Audio and video agreeing is what the mapper-driven audit above checks |

What works: quality switching across the join between the clips of `grid` in hls.js, up and down; every property above for `single`, `ladder` and `trimmed` under both players; seeking everywhere; audio-only sequences.

## What is not covered

The suite verifies structural rules the code can be held to. It is not a certification.

| Check | Status | How to run it |
| --- | --- | --- |
| Apple HLS validation (`mediastreamvalidator`, `hlsreport`) | Not run | Requires macOS and Apple's HTTP Live Streaming Tools. Point the validator at `http://<host>/hls/<asset>/master.m3u8` before a release |
| DASH-IF conformance (`DASH-IF-Conformance` tool or web validator) | Not run | Needs Java or the hosted validator. Run it on a served MPD before a release |
| Encrypted (`drm-clearkey`) and subtitled assets through a mapper | Not covered | They have in-process tests only; the mapper-driven audit has no key handling or subtitle playlists yet |
| CMAF (ISO/IEC 23000-19) profile conformance | Not claimed | The layout is CMAF-style but no CMAF validator has been run. Do not claim conformance |
| Browser playback with hls.js and dash.js | Chrome only | `make browser-test`; see above. Safari (native HLS) and Firefox are manual |
| Clear Key playback in a browser | Not covered | The browser suite has no key handling yet; `drm-clearkey` is in the example catalog |
| Device and player matrix (Safari, tvOS, Android ExoPlayer, smart TVs) | Not covered | Manual |

Until the vendor validators and browser tests are run and their output recorded here, treat the output as structurally sound and cross-checked but not certified.
