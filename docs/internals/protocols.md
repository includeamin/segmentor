# Protocols and assets

`asset.rs` and the `protocol/` module sit between the media pipeline and the HTTP layer. `asset` owns the loaded state of one media file; `protocol` turns a read-only view of it into playlist text.

## `asset.rs` : `PackagedAsset`

A `PackagedAsset` is everything the server knows about one media file, built once by `PackagedAsset::load(source, segment_duration_ms, limits)` (async; `load_local(path, ..)` opens a file first, for the CLI, tests, and benchmarks):

| Field | Meaning |
| --- | --- |
| `source` | The open `MediaSourceKind` (local file or remote object), used for payload reads |
| `index` | The `MediaIndex` from `mp4::parse` |
| `plan` | The `SegmentPlan` from `segment::plan` |
| `init_segments` | One cached init segment (`Bytes`) per `TrackKind` |
| `limits` | A copy of the limits, needed when preparing segments |
| `version` | First 8 bytes of the `moov` SHA-256, as 16 hex characters |
| `rendered` | Pre-rendered HLS master, HLS media playlists per track, and the DASH manifest |

`load` first awaits `mp4::parse` (async metadata fetch), then runs the CPU-bound rest, `assemble`, on the blocking pool: plan, build init segments from the parse's metadata, compute `version`, then render. Rendering takes a `Presentation` built from the index, plan, and version, not the asset, so the asset is constructed once with its rendered text already in place.

Main methods:

- `track(kind)` (delegates to `Presentation::track`), `init_segment(kind)`: look up a track or its init segment; a missing track is `Error::NotFound`, which the HTTP layer turns into `404`.
- `prepare_media_segment(kind, segment_index)`: find the track's `TrackSegment` in the plan and call `fmp4::prepare_media_segment` with sequence number `segment_index + 1`. An out-of-range index is `Error::NotFound`. It is CPU-only metadata work.
- `read_range(range)`: an async payload read from the source; the streaming task calls it once per chunk.
- `hls_master_playlist()`, `hls_media_playlist(kind)`, `dash_manifest()`: clone the pre-rendered `Bytes`.
- `presentation()`: returns the `Presentation` view over this asset.
- `index_bytes()`: estimated resident memory (samples, init segments, rendered text), used for the startup memory budget.

The `version` doubles as the cache key in URLs (`?v=`) and ETags. It changes whenever the `moov` box changes, and only then.

**Contributing:** anything computed from the whole sample table belongs in `load`, not in a request handler. Anything you add to `PackagedAsset` that holds memory should be counted in `index_bytes`.

## `protocol/presentation.rs` : the renderer input

`Presentation<'a>` is a `Copy` view of three borrowed things: the tracks, the `SegmentPlan`, and the version string. It is all a renderer needs and it keeps `protocol` independent of `asset`. Methods:

- `track(kind)`: find a track, or `Error::NotFound`.
- `tracks()` and `version()`.
- `track_segments(track_id)`: one track's segments in playback order.
- `bandwidth(track)`: average and peak bits per second. Average is total payload bytes over track duration; peak is the largest per-segment rate, so it reflects the burstiest segment.

Unit tests build a `Presentation` straight from the fixture (`protocol::fixtures::Loaded`) without loading a full asset.

## `protocol/hls.rs` : HLS playlists

Two pure functions from a `Presentation` to `String`, both called once at load.

**`master_playlist(presentation)`** writes an `#EXTM3U` master with `#EXT-X-VERSION:7`:

- The video track must be H.264 (`Error::Unsupported` otherwise). The codec string is `avc1.` plus profile, compatibility, and level as three hex bytes.
- If an audio track exists it is declared as an `#EXT-X-MEDIA:TYPE=AUDIO` rendition in group `audio`, with URI `audio/index.m3u8?v={version}`, and `mp4a.40.2` is appended to `CODECS`.
- One `#EXT-X-STREAM-INF` carries `BANDWIDTH` (video peak plus audio peak), `AVERAGE-BANDWIDTH` (sums of averages), `CODECS`, `RESOLUTION`, and the `AUDIO` group, followed by `video/index.m3u8?v={version}`.

**`media_playlist(presentation, kind)`** writes a VOD media playlist for one track: `#EXT-X-TARGETDURATION` is the largest segment duration rounded up to whole seconds; `#EXT-X-PLAYLIST-TYPE:VOD`, `#EXT-X-INDEPENDENT-SEGMENTS`, and `#EXT-X-MAP:URI="init.mp4?v=..."` reference the init segment; each segment gets `#EXTINF` with millisecond precision and the URI `segments/{n}/media.m4s?v=...`; the file ends with `#EXT-X-ENDLIST`.

All URLs are relative, so they resolve beneath the route the playlist was fetched from. No filesystem path ever appears in output.

## `protocol/dash.rs` : the DASH manifest

`manifest(presentation)` writes a static MPD (`type="static"`, profile `isoff-main:2011`):

- `mediaPresentationDuration` is the longest track duration in seconds.
- The video track becomes one `AdaptationSet` with `Representation id="video"`; audio, when present, becomes another with `id="audio"`, an `audioSamplingRate`, and an `AudioChannelConfiguration`.
- Each representation has a `SegmentTemplate` in that track's timescale, `startNumber="0"`, `initialization="$RepresentationID$/init.mp4?v=..."`, `media="$RepresentationID$/segments/$Number$/media.m4s?v=..."`, and a `SegmentTimeline` with one `<S d="..."/>` per segment. A timeline is used because real segment lengths vary with keyframe placement.
- `Representation@bandwidth` is the peak bitrate.

Because the representation IDs are `video` and `audio`, `$RepresentationID$/...` resolves to the same `/dash/{asset}/{track}/...` routes the server registers.

**Contributing:** the two renderers share the same fragments, so a fragment change affects both. Renderer unit tests check structure; FFmpeg decode tests in `http/tests.rs` check that players can actually play the output. Anything interpolated into text must be escaped or provably safe; today only numbers and hex strings are interpolated.
