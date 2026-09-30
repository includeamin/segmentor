# TDD 0008: Clipping and concatenation

- Status: Draft
- Created: 2026-09-30
- Updated: 2026-09-30
- Related ADRs: [ADR 0001](../adr/0001-use-fragmented-mp4-for-media-segments.md) (fragmented MP4 segments)
- Related designs: [TDD 0002](0002-asset-map-interface.md) (the mapper interface this extends), [TDD 0004](0004-broader-mp4-input-support.md) (the edit-list timeline clips are cut on), [TDD 0006](0006-trick-play-subtitles-and-renditions.md) (the composite-asset pattern this follows)

## Summary

A mapper answer may list **clips** instead of one `location`: each clip is a file and an optional time window within it. One clip trims a file. Two or more play back to back as one stream, even when they were encoded differently, for example a pre-roll ad followed by a window of a movie. Nothing is re-encoded: a clip starts on a keyframe, and a change of encoding between clips is signalled with an HLS discontinuity and a new DASH Period.

This is the feature most nginx-vod-module deployments rely on after basic packaging (its `clipFrom`, `clipTo`, and sequences), and later work builds on it: linear channels made from VOD files, ad markers, and accepting vod-module's mapping format.

The change is additive. A mapper that never sends `clips` gets byte-identical output.

## Context

An asset is one file today, or, since TDD 0006, several aligned renditions of the same content. Each file is parsed into a sample index (`MediaIndex`), planned into segments that start on keyframes, and served as fragments whose decode times (`tfdt`) come straight from the index. Nothing in that path requires the index to cover a whole file: it is a list of samples with decode times, and the planner and fragment writer work from whatever samples it holds.

Two properties of the existing code shape this design:

- **Loading is already two steps.** `PackagedAsset::load_with_subtitles` parses (async I/O) and then assembles on the blocking pool (plan, init segments, playlists). A transformation of the parsed index fits between the two without touching either.
- **Composites sit above the loader.** Renditions are a `ServedAsset::Composite` wrapping unchanged `PackagedAsset`s, which kept the heavily tested single-file path untouched. Sequences follow the same pattern.

## Goals

- Trim one file to a window, served with today's URLs and output shape.
- Play several files back to back as one HLS or DASH stream, including files with different resolutions, profiles, and encoder settings.
- Cut on keyframes predictably, without ever losing requested content at the start.
- Keep timestamps continuous across clips.
- Bound and validate every new input like the rest of the mapper answer.

## Non-goals

- Frame-accurate cuts. They need re-encoding the partial GOP, and segmentor never decodes.
- Adaptive clips (a clip with `renditions`). A later extension; the wire format below leaves room for it without a breaking change.
- Subtitles with clips, and I-frame playlists for sequences of two or more clips. See [Deferred](#deferred).
- Clips in the static `[assets.*]` catalog. Clips come from a mapper.
- Linear (live) channels, ad markers, and vod-module mapping compatibility. They build on this and get their own designs.

## Design

### Wire format

The answer gains `clips`, a third alternative to `location` and `renditions`; exactly one of the three is set.

```json
{
  "asset_id": "movie-with-preroll",
  "version": "2026-09-30-a",
  "clips": [
    { "location": { "type": "file", "path": "ads/preroll.mp4" } },
    { "location": { "type": "http", "url": "https://origin.example.net/m/movie.mp4" },
      "from_ms": 90000, "to_ms": 5490000 }
  ]
}
```

| Field | Required | Rules |
| --- | --- | --- |
| `location` | Yes | A `file` or `http` location with the same rules as a top-level `location`, including the `[remote_media]` policy |
| `from_ms` | No | Default `0`. A time on the source file's own clock, the clock its edit lists describe and subtitle cues are read against today |
| `to_ms` | No | Default: the end of the file. Must be greater than `from_ms`. A value past the end is clamped to the end, since a mapper rarely knows a duration to the millisecond |

Both times are at most `2^32 − 1` ms (about 49 days), which keeps every conversion to track ticks inside a `u64` at any 32-bit timescale. `clips` must have between one and `limits.max_clips` (default 64) entries.

One clip is a trim and is served as an ordinary asset: same URLs, no discontinuities, I-frame playlist included. Two or more form a **sequence**.

### Cutting a clip

All times below are on the served timeline of the clip's file, in the reference track's ticks. The reference track is the video track, or the first audio track when the file has no video, exactly as in the segment planner. The window's bounds are `F = from_ms + presentation_offset_ms` and `T = to_ms + presentation_offset_ms` (clamped to the reference track's end), converted to ticks. `presentation_offset_ms` is the shared offset the edit lists were resolved with (TDD 0004), which is how a time on the source's own clock becomes a time on the served timeline. A sample's presentation time is its decode time plus its composition offset.

- **Video start.** The last sync sample, in decode order, whose presentation time is at or before `F`. Up to one GOP before the requested start may be shown; nothing after it is lost.
- **Video end.** The shortest decode-order run from the start sample that contains every sample presented before `T`. A decode-order prefix is always decodable, and with B-frames it may keep a frame or two presented after `T`, never drop one before it.
- **Audio, with video.** Cut at the video's first and end decode times, rescaled to each audio track's timescale, by the rule the planner already uses: the first audio sample whose decode time is at or after the start, and likewise for the end.
- **Audio only.** The reference audio track needs no keyframe: it starts at the sample containing `F` and ends at the first sample at or after `T`. Other audio tracks follow its cuts.
- **Empty windows.** `F` at or past the reference track's end is an error naming the clip. Otherwise the window always holds at least the start keyframe, because its presentation time is at or before `F`, which is before `T`.

The result is a new `MediaIndex` holding only the kept samples, so a 30-second clip of a two-hour film keeps 30 seconds of index in memory. Track durations are recomputed from the kept samples. The source identity (and its metadata hash) is carried over unchanged, since the fragments still read the same bytes.

### Timeline

Clip *k* starts where clip *k − 1* ended, at position `S`. Every track of a clip is shifted by one amount, which keeps audio and video in sync within the clip. The amount moves the reference track's first decode time `d0` to `S + max(0, d0 − P_min)`, where `P_min` is the earliest presentation time among the kept reference samples:

- With the usual non-negative composition offsets, `P_min ≥ d0`: the first decode time lands on `S` and the first frame is presented a reorder delay later, as a file without edit lists is served today.
- With negative composition offsets, the decode times move later by the difference, so no frame is presented before `S` and no decode time is negative.

A clip's length is its reference track's end (last kept decode time plus that sample's duration, after the shift) minus `S`; the next clip starts at `S` plus that length. `S` is kept in nanoseconds and converted to each clip's own timescale when shifting, so rounding is at most one tick per clip and never accumulates. The first clip starts at zero.

Timestamps are continuous across clips rather than restarting at each one: several players handle a timestamp reset after a discontinuity poorly, and a continuous timeline gives each DASH Period a direct `presentationTimeOffset`. At a boundary, a clip's last audio frame may overlap the next clip by less than one frame (about 21 ms for AAC), which players absorb at a discontinuity.

### Compatibility between clips

Every clip in a sequence must have the same track layout: all with a video track or none, and the same number of audio tracks. The track at each position must use the same codec (the same `CodecConfig` variant: H.264 with H.264, AAC with AAC). Within that, anything may differ: resolution, profile and level, parameter sets, bitrate, frame rate, sample rate, channel count. A mismatch fails the asset, naming both clips and the track that differs.

### Loading

The mapper's clips are grouped by location. Each distinct location is parsed once, concurrently in a `JoinSet` as renditions are, and each clip is then trimmed from its file's parsed index and assembled on the blocking pool. Loading is all or nothing: any failure fails the asset, naming the clip by its position.

`PackagedAsset` gains a constructor that assembles from an already parsed (and trimmed) index; `load_with_subtitles` becomes parse followed by that constructor, so single-file loading is unchanged. Clips cut from the same file share its `MediaSourceKind`.

A single clip builds a `ServedAsset::Single` from the trimmed index. Two or more build a new `ServedAsset::Sequence` (in `src/sequence.rs`, alongside `src/composite.rs`), holding each clip's `PackagedAsset`, the global number of each clip's first segment, and the rendered playlists and manifest.

Signed-URL rotation is keyed by location: the refresher's selector generalizes from a rendition ID to "rendition or clip", and a rotation updates the one source every clip of that file shares.

### URLs

Media playlist and segment URLs are unchanged. Segment numbers are global across the sequence: `video/segments/{n}/media.m4s` is found by a binary search over the clips' first segment numbers, and its `mfhd` sequence number is the global `n + 1`, so `PackagedAsset::prepare_media_segment` takes the sequence number from its caller.

The one new route is a per-clip init segment:

```text
/hls/{asset_id}/{track}/clips/{clip}/init.mp4
/dash/{asset_id}/{track}/clips/{clip}/init.mp4
```

`{clip}` counts from zero in the order the mapper listed the clips. In a sequence, the plain `{track}/init.mp4` answers `404`, so every object has exactly one URL; for a single clip it is the only init URL, as today.

### HLS

Each media playlist lists the clips in order. Every clip starts with `#EXT-X-MAP:URI="clips/{k}/init.mp4?v={version}"`, and every clip after the first is preceded by `#EXT-X-DISCONTINUITY`. `#EXT-X-TARGETDURATION` covers the longest segment of any clip.

The master playlist has one variant:

- `BANDWIDTH` is the highest peak of any clip, and `AVERAGE-BANDWIDTH` the average weighted by clip duration.
- `RESOLUTION` is the largest clip's.
- `CODECS` lists every distinct codec string in clip order, as the specification requires ("every media format present in any Media Segment").
- An audio rendition's `LANGUAGE` and `NAME` come from the first clip whose track is not `und`, so an unlabelled pre-roll does not hide the movie's language.
- No `#EXT-X-I-FRAME-STREAM-INF` for a sequence (see [Deferred](#deferred)).

### DASH

One `Period` per clip, `id="clip-{k}"`, with `start` set to the clip's position `S`. Adaptation sets and representation IDs are as today (`video`, `audio-1`, ...). Each `SegmentTemplate` has `initialization="$RepresentationID$/clips/{k}/init.mp4?v={version}"`, `startNumber` set to the clip's first global segment number, `presentationTimeOffset` set to the clip's shifted start in the track's timescale, and the same `SegmentTimeline` as today. `mediaPresentationDuration` is the total.

### Versioning

The URL version of a trimmed or sequenced asset hashes the mapper's `version`, each clip's content hash (its metadata hash), each clip's `from_ms` and `to_ms`, and `FORMAT_REVISION`. The window must be part of the hash even for one clip: otherwise a trimmed copy and the whole file would share a `v`, and a CDN holding one's immutable segments could serve them for the other. `FORMAT_REVISION` does not change, because nothing served for an existing asset changes.

## Security and limits

- The mapper answer is validated before any fetch. More than one of `location`, `renditions`, and `clips`; an empty `clips` or one over `limits.max_clips`; a time over `2^32 − 1`; `to_ms` not greater than `from_ms`; and `subtitles` alongside `clips` are all malformed answers: `502` to players, never cached as valid.
- Every clip location passes the same `[remote_media]` policy as any location.
- Only the trimmed indexes count toward `limits.max_index_bytes`. A clip's whole-file index is held only while it is trimmed, and is bounded by the existing per-file limits (`max_samples_per_track`, `max_metadata_bytes`). Each distinct file is parsed once however many clips use it, so `max_clips` bounds the parse work.
- All tick arithmetic is checked, as it is in the planner.

## Observability

- `clip_trimmed` (info), once per clip: asset ID, clip position, requested `from_ms` and `to_ms`, and the start and end actually served in milliseconds. It explains a clip that starts earlier than asked without anyone opening the file.
- `asset_loaded` gains a clip count, and `/admin/status` reports it the way it reports renditions.
- No new metrics: the existing asset-load and registry metrics cover loading, and segment metrics cover serving.

## Testing

- **`clip::trim`** (unit, fixtures only): the start snaps back to the keyframe at or before `from_ms`; a B-frame fixture's end cut is a decodable prefix that keeps every frame before `to_ms`; audio is cut at the video's instants; an audio-only file is cut without keyframes; `to_ms` past the end is clamped; `from_ms` past the end is an error; an edit-list fixture is cut on its source clock; the shift places the clip at a given `S`, including a negative-composition-offset fixture.
- **Sequences:** global segment numbers map to the right clip and sequence number; `tfdt` is continuous across a clip boundary, checked by parsing served fragments; clips with different resolutions (`rendition-720p` then `rendition-480p`) are accepted; a video clip followed by an audio-only clip, and H.264 followed by HEVC, are rejected naming both clips.
- **Renderers:** HLS discontinuities and one `EXT-X-MAP` per clip, and the aggregated master attributes; DASH Periods with the right `start`, `startNumber`, and `presentationTimeOffset`.
- **Wire and versions:** every validation rule above; a trimmed file and the same file untrimmed get different versions; the same answer twice gets the same version.
- **HTTP:** the per-clip init route serves the right clip's init; the plain `init.mp4` is `404` in a sequence and served for a single clip.
- **End to end:** FFmpeg decodes a sequence of a pre-roll and a trimmed main clip over real HTTP, through both HLS and DASH, with no decode errors, frame counts matching the trimmed windows, and a total duration equal to the sum of the clips.
- **Regression:** the existing conformance suite must pass unchanged, which is the check that splitting the loader changed no bytes.

The existing fixtures cover all of this; no new media is needed.

## Rollout

Purely additive: no configuration change, no URL change for existing assets, no `FORMAT_REVISION` bump. Staged as:

1. The loader split and `clip::trim`, with single-clip trims end to end.
2. Sequences: `ServedAsset::Sequence`, the per-clip init route, HLS and DASH rendering.
3. Documentation: a "Clips" section in `docs/mapper-api.md`, a clip example in `examples/mapper/`, and the README's "What it does not do".

Rolling back is removing the `clips` handling; mappers that sent it get a malformed-answer error from an older build, which already rejects a missing `location`.

## Deferred

- **Subtitles with clips.** Because a clip starts on a keyframe, a mapper cannot know where the served timeline begins, so cues written for the output would be off by up to a GOP. The right design is subtitles attached to each clip, on that file's clock, which the server trims and shifts along with the media. Until then, `subtitles` with `clips` is rejected rather than served misaligned.
- **I-frame playlists for sequences.** One I-frame playlist with discontinuities and per-clip maps; a single-clip trim already has one. No wire change needed.
- **Adaptive clips.** A clip would take `renditions` instead of `location`, exactly as a top-level answer does.

## Open questions

None blocking. The deferred items above are follow-ups, not open decisions.
