# TDD 0006: Trick play, subtitles, and adaptive renditions

- Status: Draft
- Created: 2026-09-21
- Updated: 2026-09-21
- Related ADRs: None
- Related designs: [TDD 0002](0002-asset-map-interface.md) (the mapper interface this extends), [TDD 0004](0004-broader-mp4-input-support.md) (the input this builds on)

## Summary

Three product gaps were chosen for the next stage, in this order of delivery:

1. **HLS I-frame playlists**, for scrub previews and fast-forward. No mapper change, no decoding.
2. **Sidecar WebVTT subtitles**, listed by the mapper and served by segmentor.
3. **Adaptive renditions**, several source files per title served as one adaptive stream, listed by the mapper.

DRM is required but deferred to its own design, after these three. Section [DRM](#drm-later) records what these designs must not make harder.

The mapper changes are additive: a mapper that sends none of the new fields behaves exactly as today, and a segmentor that does not know the fields ignores them.

## Context

An asset is one MP4 today: one video track, some audio tracks, and a single video variant. That gives players no way to switch quality on a slow connection, no captions, and no fast scrubbing, which are the things a viewer notices first. The packaging pipeline already has what each feature needs: the sync flags of every sample, a per-request fragment writer, and a mapper that names where the media is.

## Goals

- Scrubbing that costs no decoding and no extra storage.
- Captions in HLS and DASH from files the mapper already hosts.
- One asset ID that plays as an adaptive stream when the mapper lists several renditions, and as today when it lists one.
- Every new input is bounded and validated like a media file: size limits, the `[remote_media]` policy for URLs, and errors that name what was wrong.

## Non-goals

- Transcoding to make renditions. They are separate encodes that already exist.
- DASH trick-mode adaptation sets. Only HLS I-frame playlists are built.
- Subtitle formats other than WebVTT, and converting text tracks inside the MP4.
- Live or dynamic manifests.
- DRM. See below.

## 1. HLS I-frame playlists

### Output

The master playlist gains an `#EXT-X-I-FRAME-STREAM-INF` line pointing at `video/iframes.m3u8`. That playlist has `#EXT-X-I-FRAMES-ONLY` and one entry per keyframe. Each entry is its own small resource, `/hls/{asset}/video/iframes/{n}/media.m4s`: a fragment holding that one sample, with the video track's existing init segment.

One fragment per keyframe is deliberate. A byte range into the ordinary media segment would not work for fMP4, because the segment's single `moof` describes every sample in it, so a range covering only the I-frame has no metadata for the player. The fragment writer already produces a `moof` and `mdat` for any run of samples on request, so a one-sample fragment is a `TrackSegment` covering one sample and no new machinery.

- `EXTINF` is the time from this keyframe to the next one, or to the end.
- `BANDWIDTH` is the peak I-frame bitrate: the largest keyframe in bits over the interval it represents. `AVERAGE-BANDWIDTH` uses the totals. `CODECS` is the video codec alone, and `RESOLUTION` is the video's.
- Only tracks with a video track and at least one sync sample get one.
- The playlist is rendered at load with the others, from the sync flags already in the index. It costs roughly a hundred bytes per keyframe.

### Keyframes that are not independently decodable

`stss` marks sync samples, and a sync sample is not always an IDR picture: an HEVC open-GOP file marks CRA pictures too, which cannot be decoded without earlier frames. Telling them apart needs a look at the NAL unit type. For H.264 the sync table normally marks IDR pictures and the risk is small, so the first version trusts `stss` and records the gap. See [Open questions](#open-questions).

### Testing

Each I-frame resource, prefixed with the init segment, must decode to exactly one frame with FFmpeg, for every fixture with video. Scrubbing itself is a Safari and tvOS behaviour and is checked by hand.

## 2. Sidecar WebVTT subtitles

### Mapper answer

An optional `subtitles` list, each entry:

```json
{ "language": "en", "label": "English", "default": true, "forced": false,
  "location": { "type": "http", "url": "https://origin.example.net/subs/movie.en.vtt" } }
```

`location` is a `file` or `http` location exactly as for media, and an `http` one is subject to the same `[remote_media]` policy: allowed hosts, no private addresses, no redirects. `language` is a BCP 47 tag and unique within the asset. `default` and `forced` are optional.

### What segmentor does with a file

The file is fetched when the asset loads and is held in memory, so requests never touch the origin:

- **Validation.** UTF-8, at most `limits.max_subtitle_bytes` (default 2 MiB) each and a limit in total, and it must begin with `WEBVTT`. Anything else fails the asset load with a message naming the language.
- **Timeline correction.** An asset with an edit list or a fragmented start time is moved onto a shifted timeline (see [TDD 0004](0004-broader-mp4-input-support.md)), so a cue authored against the source would appear early or late by that offset. Cue timing lines are shifted by the asset's timeline offset when the file is served. Everything else in the file is passed through unchanged.
- **Version.** The asset version covers the subtitle content, so changing a caption gives new URLs.

### Output

- **HLS.** `#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs"` per language with `LANGUAGE`, `NAME`, `DEFAULT`, `AUTOSELECT`, and `FORCED`, and `SUBTITLES="subs"` on the variant. Its playlist `subtitles/{language}/index.m3u8` lists the whole file as a single segment `subtitles/{language}/sub.vtt` with an `EXTINF` of the asset duration, which is valid for VOD.
- **DASH.** A text `AdaptationSet` with `lang`, `mimeType="text/vtt"`, and a `Representation` whose `BaseURL` is `subtitles/{language}/sub.vtt`.

Subtitles are not tracks of the MP4, so they are held beside the tracks and are not a `TrackKey`.

### Testing

Cue-shifting unit tests, including hour rollover and settings after the timing; validation rejects for every bad file; SSRF cases through the existing policy; and Chrome through hls.js and dash.js showing a cue in `video.textTracks` at the shifted time.

## 3. Adaptive renditions

### Mapper answer

Instead of a single `location`, an answer may give `renditions`:

```json
{ "renditions": [
    { "id": "1080p", "location": { "type": "http", "url": "https://origin.example.net/m/1080.mp4" } },
    { "id": "720p",  "location": { "type": "http", "url": "https://origin.example.net/m/720.mp4" } },
    { "id": "audio-en", "language": "en", "location": { "type": "http", "url": "https://origin.example.net/m/en.m4a" } } ] }
```

`location` and `renditions` are mutually exclusive, and a single-location answer is one implicit rendition, so nothing changes for existing mappers. A rendition with video is a video rendition; one with only audio is an audio rendition, which is how separate audio files, and several audio languages, are supplied. `id` is a short URL-safe label, unique in the asset.

### Structure

Each rendition is loaded as its own packaged asset: its own index, plan, init segments, and version, so all the existing parsing, limits, and caching apply to it unchanged. A composite asset holds the list and answers for the master playlist and the DASH manifest. Track keys stay the free-form names the routes already accept, so no route changes: `video-{id}` for a video rendition's video track (`video` when there is one), and `audio-{n}` for the shared audio tracks. The composite maps each key to a rendition and a real track.

### Audio

Audio is a single shared group, so that switching video quality never switches or restarts audio. It comes from the audio renditions when there are any, and otherwise from the audio of the first video rendition that has some. The audio of the other video renditions is ignored.

### Alignment

Players switch between renditions at segment boundaries, so every video rendition must have the same segments. The composite refuses the asset unless all video renditions have the same number of segments with the same start times, within one sample of the coarser rendition, and the same start offset. The error names the two renditions and the first segment that differs. Keyframe placement in the sources decides this, so it is a property of the encodes and not something segmentor can repair.

### Output

- **HLS.** One `#EXT-X-STREAM-INF` per video rendition, sorted by bandwidth, each with its own `BANDWIDTH`, `AVERAGE-BANDWIDTH`, `CODECS`, and `RESOLUTION`, all pointing at the shared audio group.
- **DASH.** One video `AdaptationSet` with a `Representation` per rendition (`id="video-{id}"`) and `segmentAlignment="true"`.
- **I-frame playlists** (section 1) are emitted for the lowest-bandwidth rendition only, since scrubbing does not need more.

### Limits and failure

`limits.max_renditions` (default 8). Each rendition counts toward `max_index_bytes` like any asset. Loading is all or nothing: if any rendition fails, the asset fails with the rendition named, so a viewer never gets a ladder with a silent gap. The composite's version hashes the mapper's version and every rendition's version.

### Testing

Fixtures made from one source at different resolutions and bitrates with a fixed keyframe interval, so they align; a fixture with misaligned keyframes for the refusal; an audio-only rendition. A conformance case, and Chrome switching level in hls.js and dash.js with playback continuing.

## DRM (later)

Not designed here. To keep it possible, these designs keep init segment generation per rendition and per track, and keep rendition and subtitle handling separate from how a sample entry is copied, because encrypted sources add `sinf`, `tenc`, and `pssh` boxes to the init segment and `senc` to fragments, and need key-system signaling in HLS and DASH. Encrypted sample entries (`encv`, `enca`) stay rejected until that design is accepted.

## Security and limits

Every new input goes through an existing gate: subtitle and rendition URLs through the `[remote_media]` policy, sizes through the new limits, counts through `max_renditions` and the subtitle limit, and parsing through the same bounded, checked-arithmetic style as the media parser. A hostile subtitle file can fail its asset and cannot allocate past its limit. I-frame resources are generated from the existing index and add no new input.

## Observability

Load events gain the number of renditions, subtitles, and keyframes served. Refusals (misaligned renditions, bad subtitles) are `asset_load_failed` with the specific reason.

## Rollout

Three independent changes, in order. The first needs no mapper change and can ship alone. The second and third extend [TDD 0002](0002-asset-map-interface.md) and the [mapper API reference](../mapper-api.md), additively. Each ships with its own tests and documentation, and none changes the output for an asset that does not use it.

## Open questions

- **IDR versus other sync samples.** Should the I-frame playlist inspect the NAL unit type of each keyframe, so open-GOP HEVC files do not list pictures that cannot decode alone?
- **I-frame streams per rendition.** Is one stream, from the lowest rendition, enough for Apple's seek bar, or should each rendition have one?
- **Default subtitle track.** When the mapper marks none as `default`, should segmentor pick none, or the first?
- **Other subtitle formats.** SRT is common in source libraries; converting it to WebVTT is small, but it should be a decision and not a surprise.
