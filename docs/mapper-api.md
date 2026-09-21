# Mapper API reference

This page is for people who write a **mapper**: the service that tells `segmentor` where the media for an asset ID lives. It is a self-contained reference. The reasoning behind it is in [TDD 0002](technical-design/0002-asset-map-interface.md).

A mapper answers one question: *where is asset X, and which version of it is current?* It never sees or returns media bytes. Playback authorization is not its job either; put viewer checks in a front proxy.

## Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/v1/assets/{asset_id}` | Resolve one asset |
| `GET` | `/v1/health` | Optional reachability probe |

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

### `304 Not Modified`

Send this when `If-None-Match` names the current version. The body is empty. A `Cache-Control: max-age=N` header sets the new reuse window. The server only sends `If-None-Match` when it already holds a usable answer, and never when the previous location has passed its `expires_at`, so a `304` can only be an answer to a valid question.

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

If the media's timeline was moved (an edit list or a late start), the server adds the same offset to every cue, so cues authored against the file's own clock stay in step with the picture. Nothing else in the file changes.

**Change `version` when a subtitle file changes.** The server reloads an asset only when its `version` or location changes, so an edited caption under an unchanged version is not picked up until the asset is evicted.

The HLS master playlist gains an `#EXT-X-MEDIA:TYPE=SUBTITLES` entry per file, served from `/hls/{asset}/subtitles/{language}/index.m3u8`, and the DASH manifest gains a text adaptation set. Both point at `/{hls|dash}/{asset}/subtitles/{language}/sub.vtt?v={version}`.

## What a `version` means to the server

The server keeps one loaded copy per asset, keyed by `(asset_id, version)`. A different `version`, or the same version at a different location (a rotated signed URL), makes it reload from the new location. There is **no grace period**: players holding URLs from the old version get `404` and recover by fetching the playlist again. Change `version` only when the media actually changes.

## Try it

A minimal mapper for local development, using only Python's standard library:

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
- Answers are small (the server's default limit is 16 KiB).
- The mapper answers quickly: the server's default per-request timeout is two seconds, with two retries.
- `version` also changes when a subtitle file changes.
- `expires_at` is set for anything signed, and re-signing keeps the same `version`.
- The mapper is reachable over `https` in production and requires the bearer token.
