# TDD 0004: Broader MP4 input support

- Status: Accepted; Phases 1 and 2 implemented
- Created: 2026-09-20
- Updated: 2026-09-20
- Related ADRs: None yet. Two are proposed in [Rollout](#rollout): edit-list timeline mapping and verbatim sample-entry pass-through
- Related designs: [TDD 0001](0001-on-demand-mp4-packaging-core.md) (the packaging core and its input contract, which this design widens)

## Implementation status

| Item | Status | Notes |
| --- | --- | --- |
| 1. Edit lists | Implemented | `src/mp4/edit.rs`. One edit, optionally after one empty edit; tail trim; leading audio drop |
| 2. Accurate errors | Implemented | Fragmented input, unsupported codec, and edit-list shape errors name what was found |
| 3. Track selection, multiple audio | Implemented | Non-media tracks skipped and logged; audio tracks are `audio-1`, `audio-2`, and so on |
| DASH `SegmentTimeline` `t=` | Implemented | |
| 4. Verbatim sample-entry pass-through | Implemented | `stsd` is copied byte for byte; `pasp` and `colr` now reach players |
| 5. In-tree container parser | Implemented | `mp4/boxes.rs`, `tables.rs`, `codec.rs`; the `mp4` crate is a dev-dependency only, used to cross-check |
| 6. New codecs, audio-only | Pending (Phase 3) | |
| 7. Fragmented MP4 input | Pending (Phase 4) | |
| Fixed `ftyp` brands | Implemented | `iso6` major, `iso6` and `mp41` compatible |

The [support matrix](#current-support-matrix) below records the state *before* Phase 1, which is what the design was written against. Since then the rows for edit lists, two audio tracks, timecode and metadata tracks, fragmented input's error message, QuickTime `.mov`, and the dropped `pasp` box have changed; the other rows still hold.

### What implementation found

Phase 2:

- **Copying is not always right: QuickTime audio entries are rewritten.** ffmpeg decodes a `.mov` whose `mp4a` entry uses QuickTime's sound description version 1 (extra fields, `esds` inside a `wave` box, a `chan` box), but Chrome's Media Source Extensions refuse it (`CHUNK_DEMUXER_ERROR_APPEND_FAILED`), so a byte-for-byte copy played in the conformance suite and failed in the browser. That one case is rewritten to the ISO layout, keeping the channel count, sample rate, and the `esds` box; every other entry is still copied verbatim. This is why browser playback is part of the check for this work and ffmpeg alone is not enough.
- **The URL version had to include a format revision.** Chrome kept serving the old init segment for the same `?v=` URL after the writer changed, because media URLs are immutable and the version came only from a hash of `moov`. The same would happen behind a CDN on any upgrade that changes the bytes served for an unchanged file. The version now hashes `moov` together with `FORMAT_REVISION` in `asset.rs`, to be bumped whenever init layout, timeline mapping, or playlist format changes.
- **`pasp` was dropped from every file, not only anamorphic ones.** FFmpeg writes a 1:1 `pasp` into ordinary output, so the old init writer discarded it everywhere; it only mattered for non-square pixels. `colr` (colour) travels the same path and is now kept too, and the conformance suite compares aspect ratio and colour between source and repackaged output.
- **A second implementation caught nothing, which is the result.** The in-tree parser matches the `mp4` crate on every sample of seven fixtures, down to the payload bytes at each offset. The crate is kept as a dev-dependency for exactly this comparison.
- **The parser and init writer need no per-codec code.** Because the sample entry is copied, only `avcC` and `esds` are read, and only for the manifest's codec string. A new codec in Phase 3 needs its configuration box read and nothing written, unless it too has a container-specific variant browsers refuse.
- **`SparseFile` shrank to `Metadata`.** With no crate wanting a `Read + Seek` view, the reader shim and the `ftyp` fetch went away, so a remote parse is one fewer range request.
- **Subtitle and other non-media tracks are skipped, not rejected.** The Phase 1 preflight already set aside every track whose handler is not `vide` or `soun`, so the matrix row saying subtitles are rejected has been out of date since then.
- **Tracks are numbered in file order** (`audio-1` is the first audio track in the file) where the crate returned them by track ID.

Phase 1:

- **The init segment had to drop `edts`.** The design did not say so. Left in, a player applies the edit list a second time on top of the shifted timestamps. Measured with FFprobe: audio started at 0.045 s instead of 0.067 s for the default-edits file, and at 1.024 s instead of 0.545 s for the delayed-audio file. The init writer now clears `edts`, and a test checks the bytes.
- **Encoder handler names are noise.** FFmpeg writes `SoundHandler` for every audio track, so HLS `NAME` is `Audio {n}`, plus the language in parentheses when the file names one, instead of the `hdlr` name.
- **The offset is real and small.** After packaging, both tracks of an FFmpeg-default file start at 0.0667 s where the source's edit list puts them at 0. The gap between tracks is preserved exactly, and for the delayed-audio file it equals the source's 0.4787 s. hls.js and dash.js play both files in headless Chrome with no errors. Safari and Firefox are untested.
- **Cover art in MP4 is not a track.** FFmpeg stores it as a `covr` metadata item, so the "single still image" skip rule only matters for QuickTime-style files and has no real-file fixture yet.
- **A timecode track was enough to fail a whole file.** Verified before the change ("sample description is not supported") and covered by a fixture and a test since.

## Summary

segmentor accepts a deliberately narrow slice of MP4: progressive files with one H.264 video track, at most one AAC-LC audio track, no edit lists, and one sample description per track. That slice was right for a first core, but it excludes most files that real encoders produce. A plain `ffmpeg -c:v libx264 -c:a aac` output is rejected today, because ffmpeg writes an edit list by default.

This design lists what is unsupported, ranks it by how many real files it blocks, and proposes a staged plan:

1. **Make ordinary files load:** edit lists, track selection, multiple audio tracks, and accurate error messages.
2. **Own the container layer:** copy sample entries verbatim into the init segment and parse the boxes we need ourselves, instead of re-serializing through the `mp4` crate. This fixes a correctness bug (pixel aspect ratio is dropped today) and is the prerequisite for new codecs.
3. **Add codecs:** HEVC, VP9, AV1, HE-AAC, AC-3/E-AC-3, Opus, FLAC, and audio-only assets.
4. **Later:** fragmented MP4 input. Encrypted sources and multiple sample descriptions stay rejected until they have their own designs. External data references are permanently rejected.

Nothing here transcodes. Every addition must keep the property that the hot path does no codec work.

## Context

### How the current support was measured

The input contract in [TDD 0001](0001-on-demand-mp4-packaging-core.md#input-contract) lists what is rejected. This design tests that list against files produced by a real encoder (ffmpeg n9.0.1, libx264, libx265, libvpx-vp9, SVT-AV1, native AAC, AC-3, libopus) and against the source. Each result below is from starting `segmentor serve` on the file and reading the load error, unless marked *code* (read from the source, not run) or *unverified*.

### Compatibility stance

The project is unreleased, so this design is free to change URLs, manifest output, configuration, and error text where that makes the result simpler or more correct. It does not carry compatibility shims. The breaking changes it makes are listed together in [Rollout](#rollout).

### Current support matrix

*State before Phase 1.*

| Input | Result | Evidence |
| --- | --- | --- |
| H.264 + AAC-LC, no edit list, `moov` first or last | Works | Fixtures; probe `ok.mp4` |
| **H.264 + AAC-LC as ffmpeg writes it by default** | **Rejected: "edit lists are not supported"** | Probe `default.mp4`: `elst` `[(144000, 1024, 1, 0)]` on both tracks |
| Same, with no B-frames | Rejected: same error | Probe `nob.mp4`: the audio priming edit alone triggers it |
| Video-only H.264 | Works | Fixture `h264-video-only.mp4` |
| 10-bit H.264 (High 10) | Loads, no warning | Probe `h10.mp4`. Most browsers cannot decode it |
| Rotation metadata (`tkhd` matrix) | Loads | Probe `rot.mp4`. Whether players honor it through HLS/DASH is *unverified* |
| Two audio tracks | Rejected: "at most one audio track" | Probe `twoaudio.mp4`; [planner.rs:46](../../src/segment/planner.rs#L46) |
| HEVC (`hvc1`) | Rejected: "sample description is not supported" | Probe `hevc0.mp4` |
| VP9, AV1 | Rejected: same error | Probes `vp9.mp4`, `av1.mp4` |
| AC-3, Opus audio | Rejected: same error | Probes `ac3.mp4`, `opus.mp4` |
| HE-AAC / HE-AACv2 | Rejected: "AAC profile other than AAC-LC" | *Code*: [parser.rs:154](../../src/mp4/parser.rs#L154). ffmpeg's native encoder cannot produce it, so no probe |
| QuickTime `.mov` (versioned `mp4a` entries) | Rejected: "mp4a box contains a box with a larger size than it" | Probe `q.mov`. This comes from the `mp4` crate |
| Fragmented MP4 input | Rejected, **with a misleading message**: "parser read outside the fetched metadata regions" | Probe `frag.mp4`. The intended "fragmented MP4 input" error is never reached |
| Audio-only file (`.m4a`) | Rejected by the edit list first. Would still fail: "exactly one video track" | Probe `audio.m4a`; [planner.rs:41](../../src/segment/planner.rs#L41) |
| Subtitle track (`tx3g`) | Rejected: "subtitle track" | *Code*: [parser.rs:96](../../src/mp4/parser.rs#L96) |
| Timed-metadata, timecode, cover-art tracks | Expected to reject the whole file | *Unverified.* Common in phone and action-camera files; see [Track selection](#track-selection) |
| Encrypted (`encv`/`enca`), external data reference, more than one `stsd` entry | Rejected on purpose | [parser.rs:277](../../src/mp4/parser.rs#L277) |

### A correctness defect, not only a gap

Accepted files can also be packaged wrongly. The init segment is built by cloning `moov` through the `mp4` crate and writing it back ([init.rs](../../src/fmp4/init.rs)). The crate only models the boxes it knows, so everything else inside the sample entry is silently dropped. Verified with `-vf setsar=4:3`:

| Box | In source | In init segment |
| --- | --- | --- |
| `pasp` (pixel aspect ratio) | 1 | **0** |
| `btrt` (bitrate) | 2 | 0 (harmless) |

A 4:3-SAR (anamorphic) source is therefore served with square pixels and renders at the wrong aspect. Colour (`colr`) and HDR boxes (`mdcv`, `clli`) would be dropped the same way; the ffmpeg build used did not write `colr` for this test, so that part is *unverified*.

### Why the `mp4` crate is the wall

`mp4` 0.14 models `avc1`, `hev1`, `vp09`, `mp4a`, and `tx3g` sample entries. It does not model `hvc1` (the tag Apple requires for HEVC), `av01`, `ac-3`, `ec-3`, `Opus`, or `fLaC`, and it has no support for compact sample sizes (`stz2`). It also cannot parse QuickTime-style `mp4a` entries. The raw preflight in [parser.rs](../../src/mp4/parser.rs) already walks boxes safely on its own, so the project has the tools to stop depending on the crate.

## Goals

- Accept files from mainstream encoders and devices (ffmpeg, HandBrake, phones, screen recorders, editors) without the operator re-muxing them first.
- Keep A/V sync exact when an edit list is present.
- Preserve everything in a sample entry that affects rendering (aspect ratio, colour, HDR, codec configuration) by copying it, not re-creating it.
- Add codecs only where the packager can produce a correct `CODECS` string and the protocol allows the codec in fMP4.
- Keep rejecting what would produce questionable output, with an error that says what was found and what to do.
- Keep every existing resource limit and the no-decode hot path.

## Non-goals

- Transcoding, or rewriting codec bitstreams. That includes converting `hev1` to `hvc1` by editing the bitstream; changing only the sample-entry tag is discussed in [Open questions](#open-questions).
- Non-MP4 containers (MKV/WebM, MPEG-TS, `.mp3`, `.flac`, `.ogg`). Files in the ISO BMFF family (`.mp4`, `.m4v`, `.m4a`, `.mov`) are in scope.
- DRM, encrypted-source pass-through, and subtitle conversion. Each needs its own design.
- Edit lists that cut or repeat media (more than one non-empty edit) or change the play rate.
- Compatibility with the current URL layout, manifest text, or `ftyp` brands. These change where the design says so.

## Design

### Priorities

Ordered by how many real files each item unblocks, with correctness defects first:

| # | Item | Why this order | Phase |
| --- | --- | --- | --- |
| 1 | Edit lists (single edit, plus a lead-in empty edit) | Blocks ffmpeg's default output, and therefore most files | 1 |
| 2 | Accurate errors, including fragmented MP4 | Cheap; today a user cannot tell what is wrong | 1 |
| 3 | Track selection: skip non-media tracks, multiple audio | Blocks phone and camera files | 1 |
| 4 | Verbatim sample-entry pass-through | Correctness defect (`pasp`); prerequisite for codecs | 2 |
| 5 | Own the container parsing | Removes the `mp4` crate limits (`.mov`, `stz2`, new codecs) | 2 |
| 6 | New codecs, audio-only | Widens what can be served | 3 |
| 7 | Fragmented MP4 input | Large change; a smaller share of sources | 4 |

### 1. Edit lists

#### What real files contain

ffmpeg's default output (probe `default.mp4`) has one `elst` per track, of the form `(segment_duration, media_time = 1024, rate 1)`. Two encoder facts explain it:

- **Audio priming.** AAC encoders emit a first frame of padding. The edit starts at `media_time` = one frame so that players skip it.
- **B-frame delay.** With B-frames the first sample's composition time is later than its decode time. The edit starts at `media_time` = that offset so presentation begins at zero.

Other producers add an *empty edit* (`media_time = -1`) at the start to delay one track relative to another.

#### Supported shapes

| Shape | Behavior |
| --- | --- |
| No `elst` | Unchanged |
| One non-empty edit, rate 1, `media_time` ≥ 0 | Supported |
| One empty edit followed by one non-empty edit, rate 1 | Supported |
| More than these, rate other than 1, dwell edits, or `media_time` < -1 | Rejected: "edit list shape not supported: *what was found*" |

#### The problem to solve

`tfdt` (base media decode time) is unsigned. In a progressive file the edit list lets the first decode time be earlier than the first presentation time. In fMP4 the decode timeline cannot go negative, so the edit cannot simply be applied by subtracting `media_time` from every timestamp.

#### Approach: one shared positive offset

Shift every track's timeline forward by the same amount so nothing is negative, and keep the tracks' relative offsets exact.

For each track *t*, let `M_t` be its edit's `media_time` (in the track timescale `ts_t`) and `D_t` the duration of its leading empty edit in seconds (zero if none). Define one offset for the whole asset:

```text
O = max(0, max over tracks of ( M_t / ts_t − D_t ))          (seconds)
shift_t = round((D_t + O) · ts_t) − M_t                       (track ticks, ≥ 0)
decode_time'(sample) = decode_time(sample) + shift_t
```

A sample with media time *T* then presents at `O + D_t + (T − M_t)/ts_t`, the same for every track. Relative sync is exactly what the edit list specified, and the whole presentation starts at `O` (about 67 ms for the ffmpeg default: two B-frame delays at 30 fps, `1024 / 15360` s; about 21 ms if only AAC priming is present) rather than at zero. HLS and DASH players tolerate a small non-zero start.

Two details:

- **Leading audio.** Audio samples that end at or before the edit start (the priming frame) are dropped from the track. A partial overlap is kept, so the error is bounded by less than one audio frame (about 21 ms at 48 kHz).
- **Trailing trim.** If the edit's `segment_duration` ends before the media does, samples that present at or after the end are dropped when they form a suffix in decode order, which is what a plain end-trim looks like. Otherwise the file is rejected as an unsupported shape.

#### Where it lives

The shift is applied once at parse time, so `Sample.decode_time` already includes it. The planner, fragment writer, and playlists then need no edit-list knowledge, which keeps the change small. Two consequences:

- The planner aligns audio to video by comparing shifted decode times, which is what it does today given shifted inputs.
- DASH `SegmentTimeline` must state the first segment's start with `t=`. Today it omits `t` and assumes zero ([dash.rs](../../src/protocol/dash.rs)), which is wrong once the first segment starts at `O`.

The asset `version` is derived from `moov` and changes automatically.

#### Alternative considered: carry the `elst` into the init segment

The init segment could contain the edit list and leave timestamps untouched. This is the most faithful option, but support is uneven across players. It is retained only as a fallback after the Testing matrix shows the shifted approach failing somewhere. This choice is proposed as ADR "edit-list timeline mapping".

### 2. Accurate errors

- Detect fragmented input (`moof` at top level, or `mvex` in `moov`) in the raw preflight before the `mp4` crate runs, and return "fragmented MP4 input is not supported". Today the crate fails first with an unrelated message.
- Every `Unsupported` error should carry what was found (the fourcc, the edit-list shape, the track handler) so an operator can act on it. "sample description is not supported" becomes "sample description `hvc1` is not supported".
- Log each rejected asset once at startup or first request, with the same detail.

### 3. Track selection

#### Skipping non-media tracks

Phone and camera files carry tracks that are not audio or video: timed metadata (`mebx`, `gpmd`), timecode (`tmcd`), chapters, and cover art. Today any such track is expected to fail the whole file (*unverified*, see the matrix).

Policy, decided per track by handler and sample entry:

| Track | Default behavior |
| --- | --- |
| `vide`/`soun` handler with a supported codec | Packaged |
| Handler is neither `vide` nor `soun` (metadata, timecode, text, hint) | Skipped, with one `track_skipped` log line naming the track and reason |
| `vide`/`soun` handler with an unsupported codec | Rejected, with the fourcc and track id in the error |
| Video track that is a still image (`jpeg`/`png` sample entry, one sample) | Skipped as cover art |

There is no setting for the third row. A file with an unplayable audio or video track fails to load with an error naming the track, which keeps the rule from TDD 0001 that questionable output fails loudly. If operators later need a lenient mode, it can be added as a separate change.

#### Multiple audio tracks

Exactly one video track stays required unless [audio-only](#6-codecs-and-audio-only-assets) is in scope. Several audio tracks become several renditions:

- HLS: one `#EXT-X-MEDIA:TYPE=AUDIO` per track in the same `GROUP-ID`, with `LANGUAGE` from `mdhd`, `NAME` by position (`Audio 1 (eng)`, because encoders write junk handler names), and `DEFAULT=YES` on the first only.
- DASH: one audio `AdaptationSet` per track, with `lang`.
- URL keys: `video`, then `audio-1`, `audio-2`, and so on in file order, for every asset including single-audio ones. One rule, no special case for the first track. The router already takes `{track}` as a path segment, so no route change is needed.
- Bandwidth and `CODECS` in the variant line continue to describe the default audio track.

More than one *video* track stays rejected. Multi-angle and alternate-resolution sets belong in the mapper's model, not in one file.

### 4. Verbatim sample-entry pass-through

Stop re-serializing `moov` through the `mp4` crate. Build the init segment from the source bytes:

- Take the raw `moov` bytes already held in `SparseFile`, and copy the `trak` boxes for the chosen track with `mvhd`, `tkhd`, `mdhd`, `hdlr`, `vmhd`/`smhd`/`nmhd`, and `dinf` as they are.
- Copy the `stsd` box **byte for byte**. This keeps `pasp`, `colr`, `mdcv`, `clli`, `btrt`, and any codec configuration box the packager does not otherwise need to understand.
- Write empty `stts`, `stsc`, `stsz`, `stco` boxes, as fMP4 requires, and the `mvex`/`trex` boxes.
- Zero the durations, as [init.rs](../../src/fmp4/init.rs) does now.
- Write a fixed `ftyp` instead of copying the source's: major brand `iso6`, compatible brands `iso6` and `mp41`. Source brands such as `qt  ` or `mp42` describe the original file, not this stream. `cmfc` is added only after the conformance tools in [conformance](../conformance.md) have been run against the output.

Box sizes and offsets are validated by the same bounded walker the preflight uses. This removes the `pasp` defect for every codec at once and is the reason new codecs need no per-codec box writers. Proposed as ADR "verbatim sample-entry pass-through".

### 5. Owning the container parsing

Replace the `mp4` crate's reader with an in-tree parser for the boxes segmentor uses: `mvhd`, `tkhd`, `mdhd`, `hdlr`, `elst`, `stsd` (entry headers and the codec configuration boxes), `stts`, `ctts` (versions 0 and 1), `stss`, `stsc`, `stsz`, `stz2`, `stco`, `co64`. All are small fixed layouts, and the preflight already contains the safe box walker with size checks. The existing fuzz target covers the new code with no changes.

This removes three current failures together: QuickTime `mp4a` entries, `stz2`, and codecs the crate does not model. It lands as one step. A partial version that keeps the crate for sample tables would leave two parsers in the tree for a short time and still fail on `.mov`. Since compatibility is not a constraint, the crate is removed from the runtime dependencies. It may stay as a dev-dependency for cross-checking in tests.

### 6. Codecs and audio-only assets

#### Codec table

Each codec needs three things: a sample entry the packager can copy, a codec configuration it can parse to produce an RFC 6381 `CODECS` string, and permission in fMP4 for the protocol. The protocol column below states the intent, and each row must be confirmed against the current Apple authoring guidance and DASH-IF profiles before it ships.

| Codec | Sample entry | Config box | `CODECS` string | Note |
| --- | --- | --- | --- | --- |
| H.264 (have) | `avc1` | `avcC` | `avc1.PPCCLL` | `avc3` (in-band parameter sets) can be accepted when `avcC` is present |
| HEVC | `hvc1`, `hev1` | `hvcC` | `hvc1.<profile>.<compat>.<tier+level>.<constraints>` | Apple requires `hvc1`; see [Open questions](#open-questions) |
| VP9 | `vp09` | `vpcC` | `vp09.PP.LL.DD` | |
| AV1 | `av01` | `av1C` | `av01.P.LLT.DD` | |
| AAC-LC (have) | `mp4a` | `esds` | `mp4a.40.2` | |
| HE-AAC, HE-AACv2 | `mp4a` | `esds` (AudioSpecificConfig, object type 5 or 29) | `mp4a.40.5`, `mp4a.40.29` | Explicit signaling only. Implicit SBR, where the config says LC but the stream has SBR, is rejected because the true sample rate is unknowable without decoding |
| AC-3, E-AC-3 | `ac-3`, `ec-3` | `dac3`, `dec3` | `ac-3`, `ec-3` | |
| Opus | `Opus` | `dOps` | `opus` | |
| FLAC | `fLaC` | `dfLa` | `fLaC` | |

MP3 in MP4, ALAC, and subtitles are not in this table on purpose.

#### Audio-only assets

An `.m4a` or an MP4 with no video track becomes a valid asset. The change is confined to the planner and the two protocol renderers:

- The planner cuts fixed windows of `segment_duration_ms` on audio sample boundaries when there is no video track. Every AAC frame is a random-access point, so no keyframe search is needed.
- HLS master playlists omit `RESOLUTION` and list only the audio codec. DASH emits a single audio `AdaptationSet`.
- The current "exactly one video track" rule becomes "one video track, or none if there is at least one audio track".

### 7. Fragmented MP4 input (later)

Already-fragmented sources (recorders, CMAF output) cannot be indexed from `moov` alone. The index has to come from `moof`/`traf`/`trun` boxes, located through `sidx` or `mfra` when present, otherwise by scanning. This is the one item that changes the metadata-only read strategy in [TDD 0001](0001-on-demand-mp4-packaging-core.md), so it needs its own design. Until then the accurate error from item 2 tells the operator to re-mux.

### What stays rejected

| Condition | Decision | Reason |
| --- | --- | --- |
| External data reference (`dref` not self-contained) | Permanent | It would let a file redirect reads outside the media root or allowed hosts |
| Encrypted source (`encv`/`enca`, `cenc`/`cbcs`) | Deferred | Needs key-system, `pssh`, and `senc` handling; separate design |
| More than one `stsd` entry | Deferred | Needs a per-sample description index and several entries in one init segment |
| Edit lists that cut, repeat, or change rate | Deferred | Would need concatenation-style planning |
| Multiple video tracks | Deferred | See [Multiple audio tracks](#multiple-audio-tracks) |
| Subtitles | Deferred | WebVTT/TTML conversion; separate design |

## Security and limits

The trust boundary is unchanged: files and remote objects are untrusted, and only `ftyp`, `moov`, and box headers are read to build the index.

- **Every new parser is bounded.** New box parsing uses the existing walker, so sizes are checked against the enclosing box and the source length, and `size == 0` and 64-bit sizes are handled the same way.
- **Verbatim copy is copy-only.** Pass-through copies bytes the packager has already length-validated. It never interprets sample-entry contents beyond the configuration boxes it must read for `CODECS`, so a hostile file cannot make it write out of range.
- **Edit lists can inflate work.** Entry counts are capped (`max_tracks` already applies per track; add a fixed cap on `elst` entries, for example 16, before shape checking).
- **Timeline math uses checked arithmetic.** `shift_t` and rescaling to the track timescale use `checked_*`, and a shift that overflows `u64` rejects the file.
- **Track skipping is not a bypass.** Skipped tracks are never read or served, and the sample-count, index-memory, and segment limits are checked on the packaged tracks only.
- **Existing limits apply to new codecs unchanged:** `max_samples_per_track`, `max_samples_per_segment`, `max_segment_bytes`, `max_index_bytes`.
- **Unsupported media tracks still fail the load,** so the default remains fail-closed.

## Observability

- Log `track_skipped` (asset, track id, handler, reason) and `edit_list_applied` (asset, track id, `M_t`, `D_t`, resulting `shift_t`) at `info` on load.
- Add the failing fourcc, edit-list shape, or handler to the existing `asset_load_failed` event, so the reason is queryable and not only in a message string.
- Count load failures by reason in the existing resolver/load metrics, using a small closed set of labels (`edit_list`, `codec`, `fragmented`, `track`, `other`) so label cardinality stays fixed, as in [metrics.rs](../../src/observability/metrics.rs).

## Testing

**Fixtures.** Extend [generate.sh](../../tests/fixtures/generate.sh). The current fixtures are all made with `-use_editlist 0`, which is why the largest gap went unnoticed. Add:

- ffmpeg default output (edit list, B-frames), and the same without B-frames
- a file with an empty leading edit and a trimmed tail
- two audio tracks with different languages
- a file with an extra timecode or metadata track, and one with cover art
- non-square SAR and tagged colour (`pasp`, `colr`)
- HEVC (`hvc1`), VP9, AV1, AC-3, Opus, HE-AAC where an encoder is available, and an `.m4a`
- a QuickTime `.mov`
- a fragmented MP4, to lock in the accurate error

**Unit tests.**

- Edit-list mapping: table-driven cases for the shift formula, including the ffmpeg defaults and empty leading edits, with sync between tracks asserted exactly and `tfdt` never negative.
- Leading-audio drop and trailing-trim rules, and every rejected shape.
- Pass-through: the `stsd` bytes in the init segment equal the source's, byte for byte. Golden test with `pasp`/`colr`.
- `CODECS` string derivation per codec, compared with FFprobe's `codec_tag_string` and profile output as the existing parser tests do.

**Round-trip checks.** Extend the FFmpeg decode suite: for each new fixture, decode through HLS and DASH and compare frame and sample counts and first-frame timestamps with the source. For edit-list files, assert A/V start times agree with the source's, to within one audio frame.

**Compatibility, currently not covered.** The [browser playback suite](0001-on-demand-mp4-packaging-core.md) is still pending. Edit-list handling and every codec depend on player behavior, so this work should not be called done without it. Minimum matrix: Chrome and Firefox through hls.js and dash.js, Safari native HLS, and the Apple HLS validator and DASH-IF conformance tool from the [conformance](../conformance.md) page. The demo player in [demo/](../../demo/) can serve as a manual harness.

**Fuzzing.** The seeded fuzz target should include the new parsers and the edit-list mapper. Add seeds from the new fixtures.

**Performance.** Re-run `make bench` before and after. Verbatim pass-through should reduce init-segment work; the edit-list shift adds one addition per sample at load.

## Rollout

| Phase | Contents | Exit criteria |
| --- | --- | --- |
| 1 | Edit lists (item 1), accurate errors (2), track selection and multiple audio (3), DASH `SegmentTimeline` `t=`; new fixtures | ffmpeg-default files play with correct sync in the browser matrix. No regression on existing fixtures. Matrix table updated |
| 2 | Pass-through init writer (4), in-tree parser (5) | `pasp`/`colr` preserved; `.mov` and `stz2` parse; the `mp4` runtime dependency removed; fuzz target updated |
| 3 | Codecs and audio-only (6), one codec at a time in the order HEVC, HE-AAC, Opus, AC-3/E-AC-3, VP9, AV1, FLAC | Each codec passes decode round-trip and the protocol validators before it is listed as supported |
| 4 | Fragmented MP4 input (7) | Own design accepted first |

**Breaking changes.** Made deliberately, and none are shimmed:

- Track URL keys become `video` and `audio-N` (was `audio`). The asset `version` changes, so every cached URL refreshes once.
- The DASH `SegmentTimeline` states the first segment start with `t=`.
- The init segment's `ftyp` is fixed, and its `stsd` is copied verbatim, so `pasp`, `colr`, and HDR boxes now appear where they were dropped.
- Presentation may begin slightly after zero for files with edit lists (see [Approach](#approach-one-shared-positive-offset)).
- Error text and the `asset_load_failed` event change to include what was found.
- The `mp4` crate leaves the runtime dependencies.

Each phase is still independent. Phase 1 can ship without Phase 2, and a codec can be reverted by removing it from the accepted table.

**Proposed ADRs**, to be written when their phase starts: "edit-list timeline mapping" (item 1, including the fallback) and "verbatim sample-entry pass-through" (item 4).

Update the matrix in [TDD 0001](0001-on-demand-mp4-packaging-core.md) and [media-pipeline](../internals/media-pipeline.md) as each phase lands, so the input contract has one source of truth.

## Open questions

- **Edit lists in players.** Does the shifted-timeline approach behave identically in Safari, Chrome, and Firefox, including seeking to the first segment? What does each do with a first `tfdt` greater than zero when the DASH manifest and HLS playlist both start there? If any player misbehaves, is carrying the `elst` in the init segment a better fallback for that player?
- **`hev1` versus `hvc1`.** Apple requires `hvc1`. Rewriting the four-byte sample-entry tag is valid only if parameter sets are never updated in-band, which cannot be proven without scanning samples. Options: accept only `hvc1`, rewrite `hev1` when `hvcC` is complete and document the assumption, or scan the first sample of each segment. Which is acceptable?
- **Ordering of new codecs.** The order above is by expected usage, but HEVC and AV1 have the widest player-support variance. Should AV1 wait for evidence that target players decode it through the protocols in use?
- **Rotation and HDR signaling.** `tkhd` matrix and HDR boxes will be preserved by pass-through, but HLS and DASH have their own attributes (`VIDEO-RANGE`, for example). Should the manifest advertise them, and how much of this is in scope here?
- **Lenient track handling.** Should an operator be able to serve the tracks segmentor understands from a file that also has an unsupported audio track (for example AC-3 next to AAC), and if so, per asset or globally?
