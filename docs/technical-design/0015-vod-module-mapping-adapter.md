# TDD 0015: nginx-vod-module mapping adapter

- Status: Draft
- Created: 2026-10-11
- Updated: 2026-10-11
- Related ADRs: None
- Related designs: [TDD 0002](0002-asset-map-interface.md) (the mapper interface this adapts to), [TDD 0006](0006-trick-play-subtitles-and-renditions.md) (renditions), [TDD 0008](0008-clipping-and-concatenation.md) (clips), [TDD 0014](0014-encrypted-source-files.md)

## Summary

A deployment that runs nginx-vod-module in mapped mode asks a **mapping service** for a JSON "media set" per asset. This design lets segmentor read that same service directly: a `vod-module` dialect of the HTTP resolver fetches the media set the way nginx does, translates it into segmentor's own mapper answer, and hands that to the existing validation. The mapping service needs no change when the deployment moves from nginx-vod-module to segmentor.

This is the first of five parts of the migration layer (see [Scope and order](#scope-and-order)). It covers single files, adaptive sets of sequences, clips with durations, sidecar captions, and track selection. DRM, URL compatibility, adaptive playlists and fallback come later.

## Context

nginx-vod-module's mapped mode (documented in its README; nothing here is derived from its source, which is AGPL-3.0, see [the research note](../research/nginx-vod-module.md)) requests `http://<upstream>/<location>/<fileuri>?<extra args>` and expects a JSON set:

- `sequences`: one to 32. Each has `clips` and optionally `id`, `language`, `label`, `default`, `bitrate`, `avg_bitrate`. A clip is a `source` (`path`, `sourceType`, `tracks`, `clipFrom`, …), a `concat`, a filter (`rateFilter`, `gainFilter`, `mixFilter`), `silence`, or `dynamic`.
- Set-level `durations`, `discontinuity`, `segmentDuration`, `playlistType`, `clipFrom`/`clipTo`, `notifications`, `closedCaptions`, and a family of live fields.
- `tracks` defaults to `v1-a1`: only the first video and the first audio track of a file.

Segmentor's mapper answer ([TDD 0002](0002-asset-map-interface.md), [mapper API](../mapper-api.md)) is shaped differently: one of `location`, `renditions` or `clips`; a required opaque `version`; no per-rendition language or track selection. Existing mapping services therefore cannot talk to segmentor.

## Goals

- Read an existing nginx-vod-module mapping service unchanged, for the shapes above.
- Reuse the HTTP resolver's transport and every validation of the native answer, so the adapter adds one translation step and no second set of rules.
- Be strict: a field the adapter cannot honor is refused with its JSON path, never silently dropped, unless the operator chooses to ignore it.
- Serve what the translation says **byte for byte** as the same asset given through the native answer.

## Non-goals

- The filter clips (`rateFilter`, `gainFilter`, `mixFilter`) and `silence`: they need decoding, which segmentor never does.
- Live (`playlistType` other than `vod`, `firstClipTime` and the rest): simulated live is its own design.
- Adaptive playlists (several sequences that each have several clips): [sub-project 2](#scope-and-order).
- `dynamic` clips and `clipIds`, which need a second upstream; DRM from `vod_drm_upstream_location`; `vod_fallback_upstream_location`; entry-URL and URL-parameter compatibility. Each is a later part.
- Reproducing nginx variables in `vod_upstream_extra_args`.

## Scope and order

The migration layer is five designs, in this order: (1) this adapter, with `tracks` and rendition overrides; (2) adaptive playlists, which changes the core model; (3) `cenc` output and the DRM bridge; (4) URL compatibility; (5) fallback, the migration guide and the shadow-comparison procedure.

## Design

### Placement and configuration

The adapter is a **dialect of the existing HTTP resolver**, not a new resolver. `type = "http"` stays; `dialect = "vod-module"` changes how the request is built and what is done to the response body before the usual parse and validation. Timeouts, retries, the bearer token and its file reload, the https rule, TTL clamps, stale-if-error, negative caching, the size cap, the location policy and the readiness probe are shared unchanged.

```toml
[resolver]
type = "http"

[resolver.http]
dialect = "vod-module"
base_url = "https://mapping.internal.example.net"
max_response_bytes = 65536            # nginx's own default is 1K

[resolver.http.vod_module]
path_template = "vod/{id}"            # after base_url; {id} is the asset ID, percent-encoded
extra_args = "key=abc"                # static query string (nginx's vod_upstream_extra_args)
host_header = "mapping.example"       # optional (vod_upstream_host_header)
file_root = "/data/media"             # prefix of a source `path`, removed to make it relative to storage.media_root
remote_base_url = "https://origin.example.net"   # where `sourceType: http` paths live (vod_remote_upstream_location)
health_url = ""                       # optional; absent means "healthy"
ignore = []                           # JSON paths to accept and drop
```

`path_template` defaults to `{id}`. Everything under `vod_module` is optional except what the mapping uses: `file_root` for file sources, `remote_base_url` for relative http paths.

The request is `GET {base_url}/{path_template}?{extra_args}` with `Accept: application/json`, `X-Request-Id`, the bearer token if one is configured, and `Host` if `host_header` is set. A `404` is "no such asset". Network errors and `5xx` are retried and covered by stale-if-error. The dialect has no asset listing, so the admin panel lists only what is loaded.

### Translation

The translation is a pure function from the parsed JSON to a native answer (as JSON text), which then goes through the existing parse and `interpret`. It first flattens each sequence into **pieces**: a `source` clip is one piece; a `concat` clip is one piece per entry of `paths`, with `basePath` prefixed.

| The mapping has | The native answer |
| --- | --- |
| 1 sequence, 1 piece, no window and no override | `location` |
| 1 sequence with a window (`clipFrom`, `clipTo`, `durations`) or several pieces | `clips`, each `{location, from_ms, to_ms}` with `to_ms = clipFrom + durations[i]` |
| 1 sequence, 1 piece, with `language`, `label` or `default` | one-element `renditions`, so the override has somewhere to live |
| Several sequences, one piece each, no window | `renditions`; the id is the sequence `id` if it is a valid rendition ID, else `s1`, `s2`… |
| Several sequences, any with several pieces or a window | refused: adaptive playlists, a later design |
| A sequence whose file ends `.vtt`, `.srt`, `.ttml` or `.dfxp` | a `subtitles` entry with its `language` and `label` |

Rules the table leaves out:

- **Windows.** With several pieces `durations` is required (as in nginx), must match them in length, and every duration must be positive. Set-level `clipFrom`/`clipTo` apply only to a single piece; across several they are refused. A window together with `language`, `label` or `default` on a lone sequence is refused too, since `clips` carry no overrides. Captions beside `clips` meet segmentor's existing rule that subtitles cannot be combined with clips, and are refused with it.
- **Paths.** `sourceType` defaults to `http` if `remote_base_url` is set, else `file`. A file `path` must start with `file_root` (the configured prefix is removed to make it relative to `storage.media_root`); with no `file_root` it must already be a valid relative path. An http `path` is joined to `remote_base_url`, or used as is if it is `http://` or `https://`. The special path `"empty"` is refused.
- **`tracks`** is always sent, defaulting to `v1-a1`. See [the answer model](#additions-to-the-answer-model).
- **Refused, with the JSON path:** `playlistType` other than `vod`; any live field; clip types `rateFilter`, `gainFilter`, `mixFilter`, `silence`, `dynamic`; `clipIds`; `notifications`; `closedCaptions`; source `encryptionKey`, `encryptionIv`, `encryptionScheme` (nginx's source encryption has no key ID, which segmentor's [source decryption](0014-encrypted-source-files.md) needs); a concat `offset` other than 0; `segmentDuration` unless it equals the server's `segment_duration_ms`.
- **Dropped with a debug log**, because segmentor computes or does not need them: `bitrate`, `avg_bitrate`, `keyFrameDurations`, `firstKeyFrameOffset`, `cache` (see below), `consistentSequenceMediaInfo`, `referenceClipIndex`, `id`. `discontinuity: false` is accepted and logged: segmentor marks every clip boundary as a discontinuity, which players handle.
- `ignore` lists JSON paths (or field names) the operator accepts dropping; it can only relax a refusal, never add behavior.

Two worked examples. An adaptive set with a language track:

```json
{"sequences":[
  {"id":"v720","clips":[{"type":"source","path":"/data/media/movie/720.mp4","tracks":"v1"}]},
  {"id":"en","language":"eng","label":"English","default":true,
   "clips":[{"type":"source","path":"/data/media/movie/en.mp4","tracks":"a1"}]}]}
```

becomes

```json
{"asset_id":"movie","version":"…","ttl_seconds":300,"renditions":[
  {"id":"v720","location":{"type":"file","path":"movie/720.mp4"},"tracks":"v1"},
  {"id":"en","location":{"type":"file","path":"movie/en.mp4"},"tracks":"a1",
   "language":"eng","label":"English","default":true}]}
```

and a playlist with a pre-roll:

```json
{"durations":[10000,0],"sequences":[{"clips":[
  {"type":"source","path":"/data/media/ad.mp4"},
  {"type":"source","path":"/data/media/movie.mp4","clipFrom":5000}]}]}
```

is refused for its zero duration, while `durations` of `[10000,60000]` gives two clips: `0–10000` of the first file and `5000–65000` of the second.

### Versions, caching and file identity

The mapping has no version, and segmentor needs one ("equal versions are assumed to be identical media"). The adapter makes it from what determines the output: a SHA-256 over the translated answer's locations (kind and identity), windows, `tracks`, rendition overrides and subtitles, and a constant bumped whenever the translator changes.

- A location's identity is its path, or its URL **without the query**: the rule segmentor already uses to tell a re-signed URL from a different object. A service that signs URLs on every request therefore does not churn the version or reload the asset.
- For a `file` location the file's size and modification time are mixed in (one `stat` per resolve), so a replaced file is noticed at the next revalidation, as nginx notices when its caches expire.
- Mapping services give no ETag, so every revalidation fetches the mapping again. When the computed version equals the loaded one the answer is `Unchanged` and nothing is reparsed.
- The TTL is `Cache-Control: max-age` if present, else `default_ttl_ms`, clamped as for any mapper. A set with `"cache": false` gets `min_ttl_ms`.
- There is no `expires_at`. When an origin rejects a read, segmentor already re-resolves once and retries, which fetches the mapping again and picks up a fresh signed URL.

### Additions to the answer model

The adapter can only be faithful if segmentor's own answer can express two things. Both are optional fields, so existing mappers are unaffected and native mappers can use them.

**`tracks`: which tracks of a file to serve.** A string beside `location`, on the answer, on each rendition and on each clip, in nginx's syntax: `v1`, `a2`, `v1-a1`; `v0` and `a0` mean every track of that kind. `v2` is the second video track in file order. A kind that is not mentioned is dropped. A selector that matches nothing selects nothing, so the default `v1-a1` still serves a video-only file; if no track at all remains the asset fails to load, naming the selection. Selected audio tracks are renumbered `audio-1`, `audio-2`… in file order, and the selection is part of the asset's own URL version. Absent means all tracks, as today. The adapter always sends `v1-a1` unless the mapping says otherwise, because nginx serves only the first video and first audio track.

**`language`, `label` and `default` on a rendition.** They apply to the audio the rendition contributes and replace what the file's `mdhd` says. `language` is a three-letter ISO 639 code, `label` at most 128 bytes with no control characters, `default` a boolean. They appear where players read them: HLS `LANGUAGE`, `NAME` and `DEFAULT`, DASH `lang` and label. An audio-only single sequence with an override has no video rendition, which the adaptive path requires, and is refused with that reason.

### Errors

Every refusal is the `502` an invalid native answer gets, with a message that names the JSON path and the problem, for example `sequences[1].clips[0].type "rateFilter" is not supported`. A message never echoes a value: mapping responses carry signed URLs and `encryptionKey` fields, so only paths and types are named.

## Security and limits

- The mapping service decides what to serve, as any mapper does, but its answer is validated like any other: path rules, location policy (private addresses, https), rendition and clip limits, ID rules.
- A file `path` must lie under `file_root` after normalization, with no `..` and no NUL. The adapter cannot widen filesystem access beyond `storage.media_root`.
- At most 32 sequences (nginx's own limit), no more pieces than `limits.max_clips`, no more renditions than `limits.max_renditions`. The response size cap and the JSON nesting limit are the shared ones.
- Secrets: no value from a mapping response appears in an error, a log line, or `/admin/status`.

## Observability

`/admin/status` reports the resolver's `dialect`. A rejected mapping logs its JSON path and is counted by the existing rejected-answer metric. The load log line names the dialect. No new metrics.

## Testing

- **Translator, table-driven:** each row and each rule above; each refusal with its exact path and no value in the message; each dropped field; window arithmetic (including `clipFrom` with `durations`); concat expansion with `basePath`; the `sourceType` and `file_root` rules; paths that try to escape; the 32-sequence and clip-count limits.
- **Documented examples:** the media sets in nginx-vod-module's documentation, translated and compared with the expected native answers.
- **End to end through the mock mapper:** each kind of mapping (single file, renditions with languages, clips with durations, `tracks` on a two-audio file, a caption sequence) must be served **byte for byte** like the same asset given through the native answer.
- **`tracks` and overrides in the native answer:** a two-audio file served with `a2`, `v1-a1`, `a0`; a selector that matches nothing; an empty selection; the renumbering; the URL version changing with the selection; language, label and default in the HLS and DASH output.
- **Versions:** a re-signed URL does not change the version; a replaced file does; a changed `tracks` does; the order of sequences does.
- **Mutations:** the window arithmetic, the `file_root` check, the `tracks` default and the refusal of an unknown clip type each get a mutation that a test must catch.
- **Not testable here:** a real nginx-vod-module. It is AGPL-3.0 and not installed, and **no real mapping response or `nginx.conf` was available when this was written**, so the translator is verified against the documented format only. Real services can differ from the README in small ways (field order, optional fields, path conventions). The strict policy turns such a difference into an error naming the JSON path, not into wrong output. The first real, sanitized mapping responses and `vod_*` directives from a deployment become fixtures, and the shadow comparison of the migration guide is the end-to-end check.

## Rollout

Additive: with no `dialect` the HTTP resolver is unchanged, and rollback is removing it. Built in three stages, each releasable:

1. `tracks`, and `language`/`label`/`default` on a rendition, in segmentor's own answer, with the track filter and the HLS and DASH output.
2. The translator, with its tests.
3. The dialect plumbing (request building, version, TTL, health), the configuration, and the documentation, including a stub of the migration guide.

## Open questions

- **Captions by extension or by content.** The adapter decides at mapping time, before the file is read, so it uses the extension. A caption file with another extension would be treated as media and fail to load; is that acceptable, or should the loader sniff?
- **A `tracks` selector that matches nothing.** nginx's behavior is not documented; this design selects nothing for that selector and fails only if the whole selection is empty. A real deployment's behavior may differ.
- **`discontinuity: false`** is accepted as a discontinuity. If a deployment relies on seamless playback across clips with the same encoding, that needs the continuous mode of the adaptive-playlist design.
- **File URIs containing slashes**, as nginx allows in `<fileuri>`: segmentor asset IDs cannot, so a mapping request for such an asset needs the URL-compatibility design.
