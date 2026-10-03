# TDD 0011: Muxed audio and video for HLS

- Status: Accepted; implemented for clear single-file assets
- Created: 2026-10-03
- Updated: 2026-10-03
- Related ADRs: [ADR 0001](../adr/0001-use-fragmented-mp4-for-media-segments.md)
- Related designs: [TDD 0001](0001-on-demand-mp4-packaging-core.md), [TDD 0006](0006-trick-play-subtitles-and-renditions.md), [TDD 0010](0010-compact-sample-index.md)

## Implementation status

Implemented as designed. With the option off, every response for every fixture is byte-identical to before. Tests check that each muxed fragment's `mdat` is exactly the separate video fragment's payload followed by the audio fragment's, for every segment, and that each `trun` points at its track's first byte. FFmpeg decodes the muxed stream with one video and one audio stream.

On the head-to-head benchmark, with servers on one core, a viewer's playlists come 8,554 times a second from muxed segmentor against 6,494 from nginx-vod-module with its response cache. Segment bandwidth is unchanged by muxing (447 against 444 MiB/s), and each segment request takes about twice as long because it carries twice the bytes.

## Summary

`packaging.hls_mux_audio` serves HLS video segments that carry the default audio track too: one `moof` with a `traf` per track, and one `mdat`. A viewer then fetches one media playlist and one stream of segments instead of two of each, as nginx-vod-module serves by default. It is on by default (see Rollout), and DASH is unchanged.

## Context

segmentor serves audio as its own HLS rendition ([TDD 0006](0006-trick-play-subtitles-and-renditions.md)). That is the better layout for several languages or several video renditions, because audio is stored once and switched independently. For the most common asset, one file with one video and one audio track, it costs requests: a viewer fetches a master, a video playlist, and an audio playlist, then two segments for every six seconds.

nginx-vod-module muxes audio into the video segments by default. On the head-to-head benchmark, it serves fewer playlist sets per second than segmentor serves playlist requests, but each set is two requests rather than three. Per viewer, nginx-vod-module is therefore 1.12 times ahead on playlists, and it halves segment requests ([benchmarks](../benchmarks.md#segmentor-vs-nginx-vod-module)).

## Goals

- An HLS presentation of a single-file asset where video segments carry the default audio track.
- Byte-for-byte the same video and audio samples, decode times, and durations as the separate renditions.
- No change with the option off, and no change to DASH, I-frame playlists, or subtitles.

## Non-goals

- Muxing for adaptive (several-file) assets, where audio comes from a different file than a video rendition, and for clip sequences. Both keep separate renditions.
- Muxing encrypted assets. A fragment with two encrypted tracks needs `senc`, `saiz`, and `saio` per `traf`, and per-track handling in the encryption step. Until then, an encrypted asset keeps separate renditions with the option on, and its load logs that it did.
- Muxed DASH representations, which dash.js does not support.

## Design

### When an asset is muxed

With `packaging.hls_mux_audio = true`, a single-file asset is muxed when it has a video track, at least one audio track, and no encryption. The first audio track, which is the default rendition, is muxed. The `asset_loaded` log line records whether it was.

### URLs

The muxed stream is its own track in URLs, `muxed`, so its responses never share a cache key with the separate renditions:

```text
/hls/{asset}/muxed/index.m3u8
/hls/{asset}/muxed/init.mp4?v={version}
/hls/{asset}/muxed/segments/{n}/media.m4s?v={version}
```

`video/...` and `audio-{n}/...` keep working, so a player holding a master from before the option changed still plays. The version is unchanged, because the content is.

### Master playlist

The variant points at `muxed/index.m3u8`, with `CODECS` naming both tracks, and `BANDWIDTH` and `AVERAGE-BANDWIDTH` the sums they already are. With one audio track, there is no audio group. With several, the group's default rendition has no `URI`, which in HLS means "the audio in the variant stream", and every other audio track keeps its own `URI` as today.

### Segments

Segment `n` of the muxed stream is segment `n` of the plan: the video track's samples and the audio samples the planner already aligned to them. The `moof` has an `mfhd` and one `traf` per track, video first, each with its own `tfdt` in its own timescale. One `mdat` follows, with the video samples and then the audio samples. Each `trun`'s data offset points at its track's first byte in the `mdat`. An empty audio part, past the end of the audio, is left out rather than written as an empty `traf`.

The byte ranges are the video ranges followed by the audio ranges. Read as they are listed, that reads the interleaved file region once per track, so streaming reads such a segment in file order instead: each window of the region is read once, the video pieces in it go out at once, and the audio pieces are held until the video is done (`interleaved_windows`, `src/http/stream.rs`). The response bytes are the same; the reads fall from 1.89 to 0.97 times the segment's size, and 2-core throughput rose from 488 to about 800 MiB/s.

### Init segment

One `moov` with both `trak` boxes, copied as for a single track, and an `mvex` with a `trex` per track.

## Security and limits

`limits.max_segment_bytes` and `limits.max_samples_per_segment` apply to the muxed fragment as a whole. Nothing else changes: the samples are the ones the separate renditions already serve.

## Observability

`asset_loaded` gains `hls_muxed: true|false`.

## Testing

- The init segment has two `trak` boxes and two `trex` entries.
- A muxed fragment's `trun` data offsets point at each track's first sample, and its payload is the video payload followed by the audio payload of the same segment number.
- FFmpeg decodes the muxed presentation with one video and one audio stream, for the same duration as the separate one.
- Master playlists for one and two audio tracks; an encrypted asset falls back to separate renditions; the option off changes nothing.
- `make bench-compare` gains a muxed segmentor variant.

## Rollout

First shipped off by default, then turned on by default once the head-to-head benchmark showed it closes the per-viewer playlist gap: a single-file asset gains nothing from a separate audio playlist, since there is only one audio track to carry either way. Changing the option changes the master playlist, which is not immutable (`max-age=60`), so players move streams on their next master fetch; the separate `video/` and `audio-{n}/` streams stay available, so a player holding an older master keeps playing.

## Open questions

- Whether muxing should be chosen per asset by the mapper as well as globally. Not needed until a deployment wants both layouts.
