# TDD 0016: Adaptive clips

- Status: Accepted; stage 1 implemented (clear content)
- Created: 2026-10-16
- Updated: 2026-10-16
- Related ADRs: None
- Related designs: [TDD 0002](0002-asset-map-interface.md) (the mapper interface), [TDD 0006](0006-trick-play-subtitles-and-renditions.md) (renditions), [TDD 0008](0008-clipping-and-concatenation.md) (clips, which deferred this), [TDD 0009](0009-common-encryption-and-drm.md), [TDD 0012](0012-hls-aes-128.md), [TDD 0013](0013-key-rotation.md), [TDD 0015](0015-vod-module-mapping-adapter.md) (the adapter this unblocks)

## Summary

Today an answer is a single `location`, a ladder of `renditions`, or a sequence of `clips`, and never a combination. This design lets **each rendition be a sequence**: a rendition takes `clips` instead of `location`, so a pre-roll followed by a movie, or two programmes back to back, can be offered at several qualities and in several audio languages as one asset. A player switches quality at any segment boundary, including across a clip boundary.

It is the shape nginx-vod-module calls an adaptive set of sequences ("several sequences that play together as an adaptive set, each sequence must have the same number of clips", from its public documentation), and it is the second part of the migration layer in [TDD 0015](0015-vod-module-mapping-adapter.md#scope-and-order). It also gives a way to trim an adaptive asset, which no answer can do today.

## Context

- [TDD 0006](0006-trick-play-subtitles-and-renditions.md): every rendition is loaded exactly as a plain asset, and the composite checks that video renditions cut their segments at the same instants, builds one shared audio group, and renders one master playlist and manifest.
- [TDD 0008](0008-clipping-and-concatenation.md): every clip is a trimmed sample index served as its own asset; a sequence numbers segments across clips, gives each clip its own init segment, marks each boundary as a discontinuity in HLS and starts a new `Period` in DASH. Its [Deferred](0008-clipping-and-concatenation.md#deferred) list says "Adaptive clips: a clip would take `renditions` instead of `location`".
- The code has both halves: `composite::assemble` (alignment, audio group, master playlist) and `sequence::build` (trimming, timeline, per-clip init). Nothing connects them, and `ResolvedAsset` makes `renditions` and `clips` mutually exclusive.
- nginx-vod-module's own documentation does not say how the master playlist orders variants or how the audio group is composed across clips, so those are decided here.

## Goals

- A rendition may be a sequence. Every rendition of one asset has the same number of clips, and quality switching works at every segment boundary of the whole timeline.
- The wire shape is **rendition-major**, as in nginx's mapping, so a rendition keeps its `id`, `language`, `label` and `default` in one place and the [mapping adapter](0015-vod-module-mapping-adapter.md) translates almost one to one.
- Reuse what exists: each (rendition, clip) is a trimmed `PackagedAsset`, alignment and the audio group are computed by the code that already does it, and the validation rules of TDD 0006 and 0008 are not duplicated.
- Loading stays all or nothing, and every error names the rendition and the clip.
- Output is deterministic and byte-stable for the same answer, as for every other asset.

## Non-goals

- I-frame playlists for sequences, subtitles together with clips, muxed HLS audio, and `discontinuity: false` (one seamless `Period` for identically encoded clips). Each is deferred by an earlier design or here, and each is refused with a clear message rather than served wrong.
- Renditions with different numbers of clips. A mapper that wants one audio file across a pre-roll and a movie supplies a clip of audio for the pre-roll too.
- Repairing mismatched encodes. As for ladders, vodnik checks and fails; it never re-encodes.
- Encryption in the first stage; see [Rollout](#rollout).

## Design

### Wire format

A rendition gives exactly one of `location` or `clips`. `clips` has the shape of [TDD 0008](0008-clipping-and-concatenation.md#wire-format); `id`, `language`, `label` and `default` stay on the rendition.

```json
{
  "asset_id": "movie",
  "version": "2026-10-16-a",
  "renditions": [
    { "id": "1080p", "clips": [
      { "location": { "type": "http", "url": "https://origin.example.net/ad/1080.mp4" } },
      { "location": { "type": "http", "url": "https://origin.example.net/m/1080.mp4" }, "from_ms": 0, "to_ms": 600000 }
    ] },
    { "id": "720p", "clips": [
      { "location": { "type": "http", "url": "https://origin.example.net/ad/720.mp4" } },
      { "location": { "type": "http", "url": "https://origin.example.net/m/720.mp4" }, "from_ms": 0, "to_ms": 600000 }
    ] },
    { "id": "audio-en", "language": "eng", "label": "English", "default": true, "clips": [
      { "location": { "type": "http", "url": "https://origin.example.net/ad/en.m4a" } },
      { "location": { "type": "http", "url": "https://origin.example.net/m/en.m4a" }, "from_ms": 0, "to_ms": 600000 }
    ] }
  ]
}
```

Rules, each checked when the answer is read, before any file is opened:

- `location` and `clips` are mutually exclusive on a rendition. An answer may not mix renditions that have `clips` with renditions that have `location`: every rendition has `clips`, or none does.
- `tracks` on a rendition that has `clips` is rejected; each clip carries its own, as in TDD 0008.
- Every rendition lists the same number of clips, at most `limits.max_clips`. A mismatch names the two renditions and their counts.
- At most `limits.max_renditions` renditions, and at most `limits.max_sequence_files` (renditions times clips, default 256) in total.
- `subtitles` beside renditions that have `clips` is rejected, for the reason TDD 0008 gives for a plain sequence.
- A clip's `encryption`, the answer's `encryption`, `hls_aes128` and `source_encryption` are rejected in this stage, with a message that points here.

A ladder whose renditions each have one clip is a **trimmed ladder**: it is served as an ordinary adaptive asset with the same URLs, with no discontinuity and no clip URLs, just as one clip is served as an ordinary asset in TDD 0008.

### Loading

The distinct files across every rendition and clip are opened and parsed once each, concurrently, however many (rendition, clip) pairs name them, as `load_clips` does today. Each (rendition, clip) is then trimmed from its file's parsed index on the blocking pool. A signed-URL refresh is keyed by the first pair that names the file, so `LocationKey` gains `RenditionClip(id, position)`.

Internally the grid is **assembled clip by clip**: for clip *k*, the trimmed assets of all renditions go through the existing `composite::assemble`, which runs alignment, classifies video and audio-only renditions and builds the audio group. A new `AdaptiveSequence` then does for those composites what `sequence::build` does for single assets: number segments across clips, place the clips on a shared timeline, and render the playlists and manifest. The wire is rendition-major and the model is clip-major, so no rule of TDD 0006 or 0008 is written twice.

### What must agree

Within clip *k*, the existing rules apply unchanged: every video rendition has the same number of segments, starting at the same instants within one sample. Across clips, these are new checks, each naming the rendition and the clip:

- **Kind.** A rendition is video or audio-only (by the tracks that come out) in every clip. It cannot be video in one clip and audio-only in the next.
- **Shape per rendition.** Within one rendition, the clips have the same track layout and codec per position, the rule of TDD 0008 applied rendition by rendition.
- **Length.** Clip *k* has the same length in every video rendition, within the longest sample of the coarser rendition. Segment starts agreeing is not enough: a rendition whose clip ends a second early would drift every later boundary.
- **Audio group.** Built per clip as today, it must have the same members in the same order in every clip, or the asset fails.

### Timeline

Every rendition's clip *k* starts at the same position `S_k`, so that a player switching quality at a boundary finds the same time on the other rendition. `S_k` is the cumulative length of the earlier clips **in the first video rendition the mapper listed** (the reference rendition), computed as in [TDD 0008](0008-clipping-and-concatenation.md#timeline), and every other rendition's clip is shifted to start there. The one-sample length tolerance above bounds the gap or overlap this leaves at a boundary, which is inside the discontinuity players already handle. Segment numbers run across the whole timeline and equal in every rendition, by the alignment rule.

**Audio-only renditions follow the video.** A video clip starts on the last keyframe at or before `from_ms`, so it may begin earlier than asked, while an audio-only file has no keyframes to snap to. Each audio-only rendition's clip *k* is therefore cut to the span of the source that the reference video rendition's clip *k* kept, and moved by the same lead, exactly as the audio of a muxed file follows its video (TDD 0008, "Cutting a clip"); its own window is ignored. Its length must then agree with the reference's within a video sample plus its own longest audio frame. This was found in review: cut from its own `from_ms`, the audio started up to a GOP away from the video it was placed beside.

### URLs

Segment and playlist URLs are unchanged. Each (track, clip) has its own init segment, as in TDD 0008:

```text
/hls/{asset_id}/video-{id}/clips/{clip}/init.mp4
/hls/{asset_id}/audio-{n}/clips/{clip}/init.mp4
/dash/{asset_id}/video-{id}/clips/{clip}/init.mp4
/dash/{asset_id}/audio-{n}/clips/{clip}/init.mp4
```

The plain `{track}/init.mp4` answers `404` for a sequence, so every object has one URL. `video/segments/{n}/media.m4s` is found by binary search over the clips' first segment numbers.

### HLS

- **Master playlist.** One `#EXT-X-STREAM-INF` per video rendition, sorted by bandwidth as for ladders, all pointing at the shared audio group. `BANDWIDTH` is the rendition's highest peak over its clips, `AVERAGE-BANDWIDTH` its average weighted by clip duration, `RESOLUTION` its largest clip's, and `CODECS` every distinct codec string over its clips, as TDD 0008 does for one variant.
- **Media playlists.** One per video rendition and one per audio group member, each listing every clip with `#EXT-X-MAP` and a leading `#EXT-X-DISCONTINUITY` after the first.
- No I-frame playlist and no muxed audio, as for sequences.

### DASH

One `Period` per clip, `id="clip-{k}"`, starting at `S_k`, each holding the video adaptation set with one `Representation` per video rendition and the audio group, with `startNumber` and `presentationTimeOffset` per representation as in TDD 0008. Every `Period` repeats the same representation IDs, so a player's ABR state survives a period change.

### Versioning

The version hashes the mapper's `version` with every (rendition, clip) pair's content and window, so trimming or replacing any one file gives new URLs, as for TDD 0006 and 0008.

### The mapping adapter

The translator of [TDD 0015](0015-vod-module-mapping-adapter.md) is not built yet; this is what it will do once it is. It refuses "several sequences, any with several pieces or a window". With this design that row becomes: several sequences with the **same number of pieces** become renditions with `clips`; the set-level `durations` apply to every sequence, as they already do for one, and a differing piece count is refused with the JSON path of the second sequence. `language`, `label` and `default` stay rendition overrides. Captions beside clips remain refused, because subtitles cannot be combined with clips.

## Security and limits

- Every bound is checked when the answer is read: clips per rendition, renditions, and the total of files. Opening more than the total is impossible, and the existing `limits.max_index_bytes` still bounds what stays in memory: each (rendition, clip) keeps only the index of its window.
- A mapper cannot make the server read more than it can already: every location passes the same policy as a single asset's, and the new routes take `{clip}` and a track the way TDD 0008's do, both bounds-checked.
- `limits.max_sequence_files` is new, default 256. A grid of 8 renditions by 32 clips fits; 8 by 64 does not, on purpose, because that would open 512 files for one asset load.

## Observability

- The load log line for an asset gains the clip count next to the rendition count, so an operator sees `8 renditions x 12 clips`.
- Errors name the rendition and the clip: for example ``rendition `720p` clip 1: ...`` and ``renditions `1080p` and `720p` diverge in clip 1 at segment 4``.
- No new metric: load duration and loaded bytes already cover it.

## Testing

- **Unit.** Wire parsing and every rejection above, one test each. Clip count mismatch, mixed `location` and `clips`, a clip's length or segment starts disagreeing across renditions, a rendition that is video in one clip and audio-only in the next, an audio group that differs between clips, and the total-files cap.
- **Golden.** HLS master, media and DASH output for a 2 renditions by 2 clips ladder with a separate audio rendition, checked in as text; the output is byte-stable.
- **Decode.** With `FFmpeg`, as the conformance suite does: every rendition decodes end to end with the frame count of its clips added up, and a stream stitched from one rendition's first clip and another's second clip decodes, which is what a player switching at the boundary does.
- **Adapter, once the TDD 0015 translator exists.** An nginx-style set of two sequences of two clips with `durations`, language and label translates and serves **byte for byte** like the same asset given natively, which is the test TDD 0015 sets for every translation.
- **Compatibility.** A ladder of one clip per rendition serves exactly as a ladder with each file trimmed, and existing single, ladder and sequence answers serve unchanged byte for byte.
- **Performance.** Load time and memory for 4 renditions by 4 clips against 4 renditions of one clip each, recorded in [benchmarks](../benchmarks.md).
- The vendor validators and the browser suite are a separate roadmap item and run on this output when they exist.

## Rollout

1. **Stage 1, clear content.** Wire format and validation, loading, `AdaptiveSequenceAsset`, HLS and DASH output, and the docs (mapper API, usage, limits, roadmap, TDD index). Answers with encryption are refused. The nginx-vod-module translator of TDD 0015 (its stages 2 and 3) is not built yet; when it is, it emits this shape, so nothing here waits on it.
2. **Stage 2, whole-asset encryption.** The answer's `encryption` (`cbcs`, one key set for every clip and rendition) and `hls_aes128`, which TDD 0012 already defines as covering every clip of a sequence and every rendition of a ladder.
3. **Stage 3, per-clip keys and key rotation** across the grid. This needs every rendition to start each key period at the same segment, which `check_key_periods` already requires of ladders.

Rolling back a stage is removing its handling: an older build rejects a rendition without `location` as malformed, and answers that never use `clips` on a rendition are unaffected.

## Deferred

- I-frame playlists for sequences, including adaptive ones.
- Subtitles together with clips.
- `discontinuity: false`, the seamless single-`Period` case for clips encoded with identical parameters.
- An audio rendition that spans clips of a different count than the video's.
- `dynamic` clips and `clipIds` (a second upstream), and live fields, which are their own designs.

## Implementation status

Stage 1 (clear content) is implemented; stages 2 and 3 (encryption) are not. What differs from the design above, or settles it:

- **Shared timeline.** The test `every_rendition_starts_each_period_at_the_same_time_and_continues_the_last` shows that every rendition's period starts at the same time, that each rendition's first fragment starts where its period says it does, and that each period starts where the one before ended, within 40 ms. So the timeline taken from the first video rendition holds on the output side; it has not been checked on real devices.
- **Audio follows the video's cut** (see Timeline). This was not in the design as first written.
- **Length tolerance.** The clip-length check uses the largest longest sample of any video rendition in the clip, not the larger of each pair, which is simpler and at most one coarse sample looser.
- **Audio numbers its own segments.** Each member of the shared audio group numbers its own segments across clips, because an audio-only rendition has its own segment plan; `startNumber` and the segment URIs of an audio member follow it, not the video's.
- **Frame counts differ by a frame between renditions.** Cut alike, two renditions encoded with different frame-reorder structures can hold one frame more or less in a clip (28 against 29 in the fixtures). The checks allow it, and the decode test compares each rendition with its own file cut the same way.
- **Black-box coverage is in-process.** The conformance suite builds its server from a static catalog of single files, which has no renditions or clips, so this feature is covered by the registry tests through the real router. An out-of-process check belongs to the verification harness (roadmap, project 2).
- The nginx-vod-module translator of TDD 0015 is not built; this design is what it will translate to.

## Open questions

- **Shared timeline on real devices.** The timeline checks out in the output (see Implementation status); whether real players switch cleanly across the boundary waits for the verification harness.
- **`max_sequence_files` default of 256.** A guess from the largest grids in the examples. It should be set from a real catalog.
- **Variant order.** Sorted by bandwidth as for ladders. nginx-vod-module's documentation does not say how it orders them, so no compatibility rule is known; confirm against a real deployment's players.
