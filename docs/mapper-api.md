# Mapper API reference

This page is for people who write a **mapper**: the service that tells `segmentor` where the media for an asset ID lives. It is a self-contained reference. The reasoning behind it is in [TDD 0002](technical-design/0002-asset-map-interface.md).

A mapper answers one question: *where is asset X, and which version of it is current?* It never sees or returns media bytes. Playback authorization is not its job either; put viewer checks in a front proxy.

## Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/v1/assets/{asset_id}` | Resolve one asset |
| `GET` | `/v1/health` | Optional reachability probe |
| `GET` | `/v1/assets` | Optional list of asset IDs, for control panels; see [List assets](#list-assets) |

`{asset_id}` is 1 to 128 ASCII letters, digits, `-`, or `_`. The server rejects any other ID before contacting the mapper, so no escaping is needed. The mapper must ignore request headers it does not know, and the server ignores response fields it does not know, so either side can add fields without breaking the other.

## Resolve an asset

```text
GET /v1/assets/big-buck-bunny
Accept: application/json
Authorization: Bearer <token>          (if configured)
If-None-Match: "2026-09-18T10:22:31Z#7"  (on revalidation)
X-Request-Id: 18d6d3516a4d8c6c-2
User-Agent: segmentor/0.1.0
```

### `200 OK`

`Content-Type: application/json`. A file on the server's media root:

```json
{
  "asset_id": "big-buck-bunny",
  "version": "2026-09-18T10:22:31Z#7",
  "ttl_seconds": 300,
  "location": { "type": "file", "path": "movies/big-buck-bunny.mp4" }
}
```

A file on an HTTP origin:

```json
{
  "asset_id": "big-buck-bunny",
  "version": "etag-9f2c",
  "ttl_seconds": 300,
  "expires_at": "2026-09-19T12:00:00Z",
  "location": { "type": "http", "url": "https://origin.example.net/movies/big-buck-bunny.mp4?sig=..." }
}
```

| Field | Required | Rules |
| --- | --- | --- |
| `asset_id` | Yes | Must equal the requested ID exactly |
| `version` | Yes | 1 to 256 visible ASCII characters (no spaces). **Any change to the media must change it**; equal versions are assumed to be identical media |
| `ttl_seconds` | No | How long the server may reuse this answer. Falls back to `Cache-Control: max-age`, then to the server's default, and is clamped to the server's minimum and maximum |
| `expires_at` | No | RFC 3339 deadline after which the *location itself* is dead, for example a pre-signed URL. The answer is never reused past it, even when the mapper is down |
| `location.type` | Yes | `file` or `http`. Anything else is rejected |
| `location.path` | For `file` | Relative to `storage.media_root`; no leading `/`, no `.` or `..` components, no NUL, at most 4096 bytes |
| `location.url` | For `http` | See [Remote locations](#remote-locations) |
| `subtitles` | No | Sidecar WebVTT files; see [Subtitles](#subtitles) |
| `renditions` | Instead of `location` | Several files served as one adaptive asset; see [Renditions](#renditions). Exactly one of `location`, `renditions`, or `clips` must be set |
| `clips` | Instead of `location` | One file trimmed, or several played back to back; see [Clips](#clips) |
| `encryption` | No | Content keys and DRM signalling; see [Encryption](#encryption). The answer then carries secrets |

### `304 Not Modified`

Send this when `If-None-Match` names the current version. The body is empty. A `Cache-Control: max-age=N` header sets the new reuse window. The server only sends `If-None-Match` when it already holds a usable answer, and never when the previous location has passed its `expires_at`, so a `304` can only be an answer to a valid question.

A `304` means nothing in the answer changed, including `encryption`: the server keeps the answer it already holds. When you re-key an asset, answer `200` with the new `encryption` object (and change `version`), never `304`.

### Errors

| Status | Meaning | What the server does |
| --- | --- | --- |
| `404` or `410` | The asset does not exist (or no longer does) | Serves `404` to players, caches the absence briefly, and drops any loaded copy |
| `401` or `403` | The server's credentials are wrong | Serves `502`, logs at `error` |
| `429` | The mapper is shedding load | Retries after `Retry-After` (capped at one second), then serves `503` |
| `5xx`, timeout, connection failure | The mapper is unhealthy | Retries with a short backoff, then serves `503`, or serves the previous answer if it is still within the stale window |
| `200` with an invalid body, wrong `asset_id`, bad `version`, unknown location type, or a policy violation | Malformed answer | Serves `502` and never caches the answer as valid |
| Any other `4xx` | Contract violation | Serves `502` |

Behavior depends only on the HTTP status, so error bodies are informational. `{"error": {"code": "...", "message": "..."}}` is a good shape.

## Health

`GET /v1/health` returning any `2xx` means healthy. It is used only when the server's optional readiness probe is enabled, in which case the server reports itself not ready while the mapper is unreachable. Mappers without this endpoint can leave the probe disabled.

## List assets

`GET /v1/assets` is optional. When a mapper implements it, the server's `/admin/status` offers the IDs it returns as `resolver.known_assets`, so the [demo player](../demo/) and [control panel](../admin/) can list them in their asset dropdowns before anyone has played them. Nothing else uses it: the server never preloads or resolves an asset because it is listed.

```text
GET /v1/assets
Accept: application/json
Authorization: Bearer <token>          (if configured)
```

```json
{ "assets": ["big-buck-bunny", "movie-with-preroll", "trailer"] }
```

List whatever is useful to browse; it need not be every asset. The server drops any ID a request could not name (the rules in [Endpoints](#endpoints)), removes duplicates, sorts the rest, and keeps at most `limits.max_assets` (default 1000). The answer is bounded like any other (`max_response_bytes`). Any failure, including a `404` from a mapper that does not implement the endpoint, a malformed body, or an outage, simply means there is no list: `known_assets` is `null` and nothing else changes. `/admin/status` asks on every call, so keep the answer cheap.

## Remote locations

An `http` location makes the server fetch media from a URL the mapper chose, so the server applies the operator's policy before any request:

- the scheme must be `https` (or `http` if the operator enabled it for development);
- the host must be in the server's `remote_media.allowed_hosts` list, compared exactly and case-insensitively;
- credentials in the URL (`user:pass@`) are refused; put access control in the query string (a signature) instead;
- literal IP addresses, and names that resolve to loopback, private, link-local, shared, or multicast addresses, are refused unless the operator allowed private addresses;
- redirects are never followed.

The origin must:

- honor `Range` requests with `206` and a correct `Content-Range`;
- send a strong `ETag`, or a `Last-Modified`, on the response. The server sends it back as `If-Range` on every read, so an object that changes while it is being read fails the read instead of mixing two versions. A weak `ETag` alone is refused.

### Signed URLs

Signed query parameters are treated as secrets: they are not logged at `info` and never appear in error responses. The server handles URL rotation in three ways, so a mapper only has to issue a fresh signature when asked:

- **Refresh ahead.** When an answer has `expires_at`, the server re-asks the mapper *before* the deadline (`resolver.http.refresh_margin_ms`, default 30 seconds early, but never before half the remaining lifetime has passed). The mapper is asked at the next request after that point, so playback that is in progress keeps refreshing itself.
- **In-place rotation.** If the new answer has the **same `version`** and names the same object (same scheme, host, port, and path, differing only in the query string), the server keeps the loaded asset and simply uses the new URL from the next read on. Nothing is reparsed, and streams already in flight pick up the new signature on their next chunk. Any other change (a different version, path, or host) reloads the asset.
- **Recovery on rejection.** If the origin answers `401`, `403`, or `410` to a read (an expired or revoked signature, or clock skew), the server asks the mapper once for a fresh answer, without an `If-None-Match`, and retries the read. Concurrent rejections share one lookup. If the mapper cannot provide a working location, the response ends in an error rather than retrying forever. This recovery is disabled while an asset is first loading; a rejected location at that point is a `502`.

So: set `expires_at` on anything signed, keep the **same `version`** when you only re-sign, and answer unconditional requests (no `If-None-Match`) with a full body and a new signature. A `304` is only appropriate while the current signature is still valid.

## Subtitles

An answer may attach WebVTT subtitle files to the asset:

```json
{
  "asset_id": "movie",
  "version": "2026-09-21-a",
  "location": { "type": "file", "path": "movie.mp4" },
  "subtitles": [
    { "language": "en", "label": "English", "default": true,
      "location": { "type": "file", "path": "subs/movie.en.vtt" } },
    { "language": "fr", "label": "Français", "forced": false,
      "location": { "type": "http", "url": "https://origin.example.net/subs/movie.fr.vtt" } }
  ]
}
```

| Field | Required | Rules |
| --- | --- | --- |
| `language` | Yes | A BCP 47 tag of letters, digits, and hyphens, starting with a letter, at most 35 characters. Unique within the asset, ignoring case. It appears in the URLs |
| `label` | No | What a player shows the viewer. Defaults to the language. At most 128 bytes, no control characters |
| `default` | No | The player selects this one unless the viewer chose otherwise. At most one entry may set it |
| `forced` | No | The track is meant to be shown even when the viewer has not asked for subtitles |
| `location` | Yes | A `file` or `http` location with the same rules as the media's, including the `[remote_media]` policy. An `http` origin must support ranged requests, as media origins do |

The server fetches each file when the asset loads and keeps it in memory, so playback never touches the subtitle origin. A file must be UTF-8, must begin with `WEBVTT`, and must have readable cue timing lines. It is limited by `limits.max_subtitle_bytes` (2 MiB), `limits.max_subtitles_total_bytes` (8 MiB per asset), and `limits.max_subtitles` (16). **One bad file fails the whole asset** with the language named, so a viewer never gets a language that is silently missing.

Cue times are read as times on the source file's own clock, the one its edit lists describe. Packaging can move a file onto a later timeline so that no timestamp is negative (this is what an edit list that trims encoder delay does, and it is typically a few tens of milliseconds), and the server adds that same offset to every cue so they stay in step with the picture. A video that simply starts late, through a leading empty edit, is not an offset: the cues were written against a clock that already includes that gap, so they are left alone. A fragmented file's timeline starts at zero and cues are not moved. Nothing else in the file changes.

**Change `version` when a subtitle file changes.** The server reloads an asset only when its `version` or location changes, so an edited caption under an unchanged version is not picked up until the asset is evicted.

The HLS master playlist gains an `#EXT-X-MEDIA:TYPE=SUBTITLES` entry per file, served from `/hls/{asset}/subtitles/{language}/index.m3u8`, and the DASH manifest gains a text adaptation set. Both point at `/{hls|dash}/{asset}/subtitles/{language}/sub.vtt?v={version}`.

## Renditions

Instead of a single `location`, an answer may give several files as one adaptive asset:

```json
{
  "asset_id": "movie",
  "version": "2026-09-21-a",
  "renditions": [
    { "id": "1080p", "location": { "type": "http", "url": "https://origin.example.net/m/1080.mp4" } },
    { "id": "720p",  "location": { "type": "http", "url": "https://origin.example.net/m/720.mp4" } },
    { "id": "audio-en", "location": { "type": "http", "url": "https://origin.example.net/m/en.m4a" } }
  ]
}
```

`location` and `renditions` are mutually exclusive: a single-`location` answer is one implicit rendition, so nothing changes for a mapper that never sends `renditions`.

| Field | Required | Rules |
| --- | --- | --- |
| `id` | Yes | A short URL-safe label (letters, digits, hyphens), at most 32 bytes, unique within the asset. It appears in URLs as `video-{id}` |
| `location` | Yes | A `file` or `http` location with the same rules as a single asset's |

The server does not ask what kind a rendition is; it loads every one exactly as it would load a single-file asset, and looks at what tracks came out. **A rendition with a video track is a video rendition. One with only audio, and no video, is an audio rendition** — this is how separate audio files, and several audio languages, are supplied.

**Video renditions must be cut alike.** Every video rendition must have the same number of segments, starting at the same instants (within about one frame of the coarsest rendition), so a player can switch between them at a segment boundary. This falls out of using the same keyframe interval to encode each one; segmentor cannot repair renditions that do not already agree. A mismatch fails the whole asset, naming the two renditions and where they diverge.

**Audio is one shared group, so switching video quality never restarts it.** It comes from the audio renditions when there are any (in the order listed, numbered `audio-1`, `audio-2`, ...), and otherwise from the first video rendition that has audio; the audio of every other video rendition is ignored. **Loading is all or nothing:** if any rendition fails — cannot be fetched, fails to parse, or breaks alignment — the whole asset fails with that rendition named, rather than serving a ladder with a silent gap. At most `limits.max_renditions` (default 8) may be listed; each rendition counts toward `limits.max_index_bytes` like any asset, and the composite's version hashes the mapper's `version` together with every rendition's own content, so replacing one file's bytes under an unchanged mapper `version` is still caught.

A video rendition's own URL is `video-{id}`, for example `/hls/{asset}/video-{id}/index.m3u8`; the shared audio group stays `audio-{n}`, exactly as for a single-file asset. The HLS master playlist gains one `#EXT-X-STREAM-INF` per video rendition, sorted by bandwidth, all pointing at the shared audio group; the DASH manifest gains one `Representation` per video rendition. Sidecar subtitles and HLS I-frame playlists (the latter built from the lowest-bandwidth rendition only) both work the same as for a single-file asset.

## Clips

Instead of a single `location`, an answer may list clips: each a file and an optional window of it. One clip trims a file. Two or more play back to back as one stream, even when they were encoded differently, for example a pre-roll ad followed by part of a movie.

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
| `location` | Yes | A `file` or `http` location with the same rules as a single asset's |
| `from_ms` | No | Default `0`. A time on the file's own clock, the one subtitle cues are read against |
| `to_ms` | No | Default: the end of the file. Must be later than `from_ms`; a value past the end is clamped to the end |

Both times are at most 4294967295 ms, and an answer may list at most `limits.max_clips` (default 64) clips.

**Nothing is re-encoded, so a clip starts on a keyframe:** the last one shown at or before `from_ms`, so nothing you asked for is lost, but up to one keyframe interval before it may be shown. The end cuts at `to_ms` to the frame. Audio is cut at the same instants as the video. The `clip_trimmed` log line records, for every clip, the window asked for and the window served.

**Clips must be alike in shape, not in encoding.** Every clip must have a video track or none, and the same number of audio tracks, with the same codec in each (H.264 with H.264, AAC with AAC). Resolution, profile, bitrate, frame rate, and sample rate may all differ. A mismatch fails the asset, naming both clips.

**One clip is served as an ordinary asset**, with the same URLs as a single `location`. **Two or more form a sequence:** the HLS media playlists carry an `#EXT-X-DISCONTINUITY` and a fresh `#EXT-X-MAP` at each clip boundary, and the DASH manifest has one `Period` per clip. Segment numbers run across the whole sequence. Each clip has its own init segment at `/{hls|dash}/{asset}/{track}/clips/{n}/init.mp4` (counting from 0), and the plain `{track}/init.mp4` does not exist for a sequence. Sequences have no I-frame playlist.

**Loading is all or nothing:** if any clip's file cannot be read or parsed, its window holds no media, or it does not match the others, the whole asset fails with that clip named. A file named by several clips is read once. The URL version hashes the mapper's `version` with every clip's content and window, so trimming the same file differently always gives new URLs.

`subtitles` cannot be combined with `clips` yet: the mapper cannot know where a keyframe-aligned clip begins, so cues written for the output would drift. Such an answer is rejected.

## Encryption

An answer may carry an `encryption` object. segmentor then encrypts what it serves with Common Encryption in the `cbcs` scheme, and signals the DRM systems you name in the HLS and DASH manifests. One set of segments plays under Widevine, FairPlay, PlayReady, and Clear Key. The object applies to the whole asset: every track, every rendition, and every clip.

```json
{
  "asset_id": "movie",
  "version": "2026-10-01-a",
  "location": { "type": "file", "path": "movies/movie.mp4" },
  "encryption": {
    "scheme": "cbcs",
    "keys": [
      { "tracks": "all", "key_id": "0123456789abcdef0123456789abcdef", "key": "00112233445566778899aabbccddeeff" }
    ],
    "systems": [
      { "system_id": "edef8ba9-79d6-4ace-a3c8-27dcd51d21ed", "pssh": "<base64 pssh box>", "license_url": "https://license.example.net/widevine" },
      { "system_id": "94ce86fb-07ff-4f43-adb8-93d2fa968ca2", "hls_uri": "skd://movie" },
      { "system_id": "9a04f079-9840-4286-ab92-e65be0885f95", "pssh": "<base64 pssh box>", "license_url": "https://license.example.net/playready" }
    ]
  }
}
```

| Field | Rules |
| --- | --- |
| `scheme` | `cbcs`. Anything else is rejected |
| `keys` | One entry with `"tracks": "all"` (or no `tracks`), or one `"video"` and one `"audio"` entry, for example `{ "tracks": "video", … }, { "tracks": "audio", … }`. Every track of the asset must end up with exactly one key. FairPlay needs one key for all tracks: an answer listing FairPlay with separate video and audio keys is rejected |
| `key_id`, `key` | 16 bytes each, as 32 hex digits |
| `iv` | Optional, 16 bytes as 32 hex digits: the constant IV declared in the init segment. When absent, it is the first 16 bytes of SHA-256(`"segmentor cbcs iv"` followed by `key_id`), so every replica derives the same one. It is never derived from the key |
| `systems` | At most 8. Each names a DRM system by its standard system ID (a UUID) |
| `pssh` | Optional base64 of a complete, well-formed `pssh` box whose system ID matches the entry's `system_id`, at most 16 KiB decoded |
| `license_url` | Optional `https` URL. Written into the DASH manifest as `dashif:Laurl`, and used as Clear Key's HLS key URI |
| `hls_uri` | The HLS key URI. Required for FairPlay (for example `skd://movie`); no quotes or control characters, at most 2048 bytes |

**What each system needs:**

| System | System ID | HLS signalling | Needs |
| --- | --- | --- | --- |
| Widevine | `edef8ba9-79d6-4ace-a3c8-27dcd51d21ed` | `SAMPLE-AES` key line with a data URI of the `pssh` and the key ID | `pssh`; `license_url` for DASH |
| FairPlay | `94ce86fb-07ff-4f43-adb8-93d2fa968ca2` | `SAMPLE-AES` key line with `com.apple.streamingkeydelivery` | `hls_uri`; one key for all tracks (`"tracks": "all"`) |
| PlayReady | `9a04f079-9840-4286-ab92-e65be0885f95` | `SAMPLE-AES` key line with a UTF-16 data URI of the PlayReady object taken from the `pssh` | `pssh`; `license_url` for DASH |
| Clear Key | `e2719d58-a985-b3c9-781a-b030af78d30e` | `SAMPLE-AES` key line with `org.w3.clearkey` and the `license_url` | `license_url` for HLS; nothing for DASH |

**Clear Key in browsers.** dash.js plays Clear Key from the manifest's `default_KID` with keys it is given directly, so `{ "system_id": "e2719d58-a985-b3c9-781a-b030af78d30e" }` alone is enough there. hls.js asks the browser's Clear Key CDM for a licence from the init data in the init segment, and Chrome only recognises the W3C common system ID `1077efec-c0b2-4d02-ace3-3c1e52e2fb4b` for that. For HLS, therefore, list Clear Key with its `license_url` and add a second system with that common ID and a `pssh` box listing the key ID.

A Widevine or PlayReady entry without a `pssh` gets no HLS key line: HLS signals both from the `pssh`, so such an entry is signalled in DASH only.

Any other system ID is accepted and signalled in DASH only (its `pssh` and `license_url` go into its `ContentProtection`); HLS has no standard form for it. The DASH manifest carries a `mp4protection` `ContentProtection` with `cenc:default_KID`, then one per system.

**segmentor never contacts a licence server.** It encrypts with the keys you give it and tells players where to get a licence; the player fetches the licence from your DRM vendor. The `pssh` boxes and URLs are yours to produce.

**Codecs.** H.264 video and AAC, AC-3, and E-AC-3 audio can be encrypted. HEVC encryption is not supported yet: an asset with HEVC and an `encryption` object fails with "HEVC encryption is not supported yet". VP9, AV1, Opus, and FLAC cannot be encrypted. A source file that is itself encrypted is still rejected.

**A malformed `encryption` object makes the whole answer malformed**: a `502` to players, never cached as valid, like any other bad answer. That covers an unknown scheme, a bad hex length, keys that do not cover every track, more than eight systems, a `pssh` that is not a well-formed box or names another system, a `license_url` that is not `https`, a FairPlay entry without `hls_uri`, and FairPlay listed with separate video and audio keys. Error messages and logs never contain key material.

**The URL version covers the keys.** The key (as its SHA-256), the key IDs, the IVs, and the systems are hashed into the version in every URL. An answer whose keys changed therefore reloads the asset and hands out new URLs, even under an unchanged mapper `version`, so no player mixes old and new segments. That holds only when the mapper answers `200` with the new keys: a `304` tells the server nothing changed, and it keeps the old keys. Re-key with a `200` and a new `version`.

**Large `pssh` boxes need a larger answer limit.** The server rejects mapper answers over `resolver.http.max_response_bytes` (16 KiB by default). A 16 KiB `pssh` is about 22 KiB in base64, so an answer carrying large `pssh` boxes, or several, can exceed it; raise the limit to fit.

**The answer now carries secrets.** The mapper must be reached over `https` and must require the bearer token; do not use `allow_insecure_mapper` with real keys.

## What a `version` means to the server

The server keeps one loaded copy per asset, keyed by `(asset_id, version)`. A different `version`, or the same version at a different location (a rotated signed URL), makes it reload from the new location. There is **no grace period**: players holding URLs from the old version get `404` and recover by fetching the playlist again. Change `version` only when the media actually changes.

## Try it

[`examples/mapper/`](https://github.com/includeamin/segmentor/tree/main/examples/mapper) is a
runnable version of the mapper below, and
[`docker-compose.dev.yml`](https://github.com/includeamin/segmentor/blob/main/docker-compose.dev.yml)
at the repository root wires it up to segmentor and to the [demo player](../demo/) and
[control panel](../admin/) with one command:

```sh
docker compose -f docker-compose.dev.yml up --build
curl http://127.0.0.1:3000/hls/sample/master.m3u8
```

The mapper itself, using only Python's standard library:

```python
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

CATALOG = {"movie": {"version": "v1", "path": "movies/movie.mp4"}}

class Mapper(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/v1/health":
            self.send_response(200); self.end_headers(); return
        asset = CATALOG.get(self.path.rsplit("/", 1)[-1]) if self.path.startswith("/v1/assets/") else None
        if asset is None:
            self.send_response(404); self.end_headers(); return
        body = json.dumps({
            "asset_id": self.path.rsplit("/", 1)[-1],
            "version": asset["version"],
            "ttl_seconds": 60,
            "location": {"type": "file", "path": asset["path"]},
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

HTTPServer(("127.0.0.1", 9911), Mapper).serve_forever()
```

Point the server at it (`allow_insecure_mapper` is for development only; production mappers use `https`):

```toml
[storage]
media_root = "/srv/vod"

[resolver]
type = "http"

[resolver.http]
base_url = "http://127.0.0.1:9911"
allow_insecure_mapper = true
```

Then `curl http://127.0.0.1:3000/hls/movie/master.m3u8`.

## Checklist for mapper authors

- The response `asset_id` echoes the request.
- `version` changes whenever the media changes, and only then.
- Removed assets answer `404` or `410`, not `200`.
- Answers are small (the server's default limit is 16 KiB; raise `resolver.http.max_response_bytes` for large `pssh` boxes).
- A re-keyed asset is answered with `200` and a new `version`, never `304`.
- The mapper answers quickly: the server's default per-request timeout is two seconds, with two retries.
- `version` also changes when a subtitle file changes, or when any rendition's file changes.
- Video renditions are encoded with the same keyframe interval, so their segments align.
- `expires_at` is set for anything signed, and re-signing keeps the same `version`.
- `from_ms`/`to_ms` are on the file's own clock, and a clip may start up to one keyframe interval early.
- The mapper is reachable over `https` in production and requires the bearer token.
- Keys only travel over `https`: an answer with `encryption` is never served over plain HTTP outside development.
