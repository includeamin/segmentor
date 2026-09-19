# HTTP server

The `src/http/` module (plus `observability/metrics.rs`), built on Axum 0.8, Tokio, and Tower.

| File | Contents |
| --- | --- |
| `mod.rs` | `serve`: bind, wire shutdown, grace timer |
| `server.rs` | The accept loop: connection cap, header-read timeout, graceful drain |
| `state.rs` | `AppState`, startup asset loading, job-slot acquisition |
| `router.rs` | Route table and layer order |
| `middleware.rs` | `request_id`, `record_metrics`, `enforce_header_limit`, `shed_load` |
| `handlers/` | `health.rs` (health, ready, metrics), `playlist.rs`, `media.rs`; `parse_track` in `mod.rs` |
| `stream.rs` | `StreamJob`, the streaming task |
| `range.rs` | `ByteInterval`, `Range` and `If-Range` parsing, `416` |
| `validators.rs` | Entity tags and `If-None-Match` |
| `cors.rs` | `cors_layer` |
| `error.rs` | `HttpError` |
| `shutdown.rs` | `shutdown_signal` |
| `tests.rs` | Router-level tests |

The sections below follow the request path.

## `AppState`

Cheaply cloneable shared state passed to every handler and middleware:

| Field | Purpose |
| --- | --- |
| `assets` | `Arc<HashMap<String, Arc<PackagedAsset>>>`, immutable after startup |
| `segment_jobs` | Semaphore of `limits.max_segment_jobs` source-read slots |
| `request_slots` | Semaphore of `limits.max_concurrent_requests` handler slots |
| `metrics` | `Arc<Metrics>` |
| `ready` | Readiness flag, cleared when shutdown starts |
| `cors` | Optional prebuilt CORS layer |
| timeouts and sizes | Queue timeout, idle timeout, request timeout, chunk size, header limit |

`AppState::load(config)` (in `state.rs`) builds the CORS layer (`cors_layer`), loads every asset on a `rayon` pool, checks total `index_bytes()` against `limits.max_index_bytes`, and returns the state. It is synchronous and blocks the caller while it parses, which is fine because it runs before the listener exists. `asset(id)` looks up an asset or returns a `404` error.

## `serve` and shutdown

`serve(config)` loads state, binds the listener, logs `service_ready`, and runs `server::serve_connections` with the connection limits and a shutdown future.

### `server.rs`: the accept loop

`serve_connections` replaces `axum::serve` because that offers no header-read timeout or connection cap. For each accepted socket it takes a permit from a semaphore of `limits.max_connections` (closing the socket and counting a rejection if none is free), enables `TCP_NODELAY`, and spawns a task that runs hyper's auto (HTTP/1 and HTTP/2) connection with the router as its service. The permit and the open-connection gauge guard live for the task. hyper's HTTP/1 `header_read_timeout` (`limits.header_read_timeout_ms`, with a `TokioTimer`) closes connections that are slow to send headers or idle between requests. Accept errors that name one connection are skipped; others are logged with a one-second back-off. On shutdown the loop stops accepting and awaits `GracefulShutdown`, which closes idle connections and waits for active ones.

The shutdown future passed to `serve_connections` waits for `shutdown_signal()` (`SIGINT`, or `SIGTERM` on Unix), clears `ready` so `/ready` returns `503`, and sleeps `shutdown_delay_ms` before the accept loop stops. A `tokio::select!` races the server against a timer that starts when shutdown begins and fires after `shutdown_delay_ms + shutdown_grace_ms`, so a stuck stream cannot block exit forever.

## `router` and middleware

`router(state)` registers routes, then adds layers. In Axum the **last** `.layer()` call is the **outermost**, so the order in code is the reverse of the request path.

| Layer (outermost first) | What it does |
| --- | --- |
| `request_id` | Keeps a valid incoming `X-Request-Id` or generates one, and echoes it on the response |
| `TraceLayer` | Creates an `info`-level `request` span carrying the request ID; logs request and response at `debug` |
| `record_metrics` | Records route, status, and time to headers; the in-flight gauge is a guard that survives cancellation |
| CORS (optional) | Adds `Access-Control-*` headers; sits outside the limit layers so their errors carry them |
| `shed_load` | `try_acquire` a request slot or return `503` with `Retry-After`; `/health`, `/ready`, `/metrics` bypass it |
| `enforce_header_limit` | Sums header name and value bytes; over `max_request_header_bytes` returns `431` |
| `TimeoutLayer` | Returns `408` if a handler takes longer than `request_timeout_ms` to produce headers |

The timeout does not cover body streaming; `response_idle_timeout_ms` does (below).

Routes:

```text
GET|HEAD /health  /ready  /metrics
/hls/{asset_id}/master.m3u8
/hls/{asset_id}/{track}/index.m3u8
/hls/{asset_id}/{track}/init.mp4
/hls/{asset_id}/{track}/segments/{segment_index}/media.m4s
/dash/{asset_id}/manifest.mpd
/dash/{asset_id}/{track}/init.mp4
/dash/{asset_id}/{track}/segments/{segment_index}/media.m4s
```

HLS and DASH share the `init_segment` and `media_segment` handlers.

## Handlers

### Playlists and manifests

`master_playlist`, `media_playlist`, and `dash_manifest` look up the asset, build the ETag, return `304` if `If-None-Match` matches, and otherwise return the pre-rendered `Bytes` with `max-age=60`. They do no per-request computation.

### `init_segment`

Requires the matching `?v=` (`VersionQuery::require`), then checks `If-None-Match`, then serves the cached init `Bytes`, honoring `Range`, with `immutable` caching and the track-specific content type (`video/mp4` or `audio/mp4`).

### `media_segment`

The most involved handler:

1. Look up asset, require `?v=`, parse the track, and answer `304` if the ETag matches.
2. **Prepare** the segment on the blocking pool (`prepare_media_segment`): this yields the header bytes, source ranges, and total length without reading payload.
3. Compute the requested byte interval from `Range` and `If-Range`; `416` if unsatisfiable.
4. If the method is `HEAD`, return headers with an empty body. No job slot, no task.
5. Otherwise acquire the first job slot (`503` on queue timeout), spawn a `StreamJob`, and return `Body::from_stream` over a two-item channel.

The response's `Content-Length` and `Content-Range` are known up front because the header length and payload length are known from metadata.

### `StreamJob`

An async task that produces the body. Its `stream` method:

1. Sends the part of the header (`moof` plus `mdat` header) that overlaps the requested interval.
2. Walks the prepared source ranges. Byte positions in the response are "virtual offsets" (header first, then each range in order); for each range it intersects the requested interval and reads that overlap in chunks of `stream_chunk_bytes`.
3. For each chunk: `read` acquires a job slot (the first read reuses the slot acquired by the handler), runs `PackagedAsset::read_range` on the blocking pool, **releases the slot**, then `send` pushes the chunk into the channel under `response_idle_timeout_ms`.

Failure handling: a source-read or permit failure sends one generic `io::Error` into the body (so the connection aborts instead of ending cleanly short) and counts an `error` abort. A full channel for longer than the idle timeout drops the stream and counts `idle`. A closed channel means the client left and counts `client`.

Invariants worth preserving: a slot is never held while waiting on the client; every send is time-bounded; nothing here reads more than the requested interval.

## Conditional and range helpers

- `entity_tag(asset, resource)` builds a strong ETag `"<version>-<resource>"`.
- `not_modified` implements `If-None-Match` (tag lists, weak tags, `*`).
- `requested_range(headers, total, etag)` parses one `bytes=` range, including suffix `bytes=-n`. `If-Range` that differs from the ETag means "ignore the range". Multi-range and malformed ranges are `Err(())`, which handlers turn into `416` via `range_not_satisfiable`.
- `ByteInterval { start, end }` is half-open, with `overlap` used by `StreamJob`.
- `media_response_builder` sets `200` or `206`, content type, `immutable` cache control, `Accept-Ranges`, `Content-Length`, `Content-Range` when partial, and the ETag.

## Errors

`HttpError { status, message }` with constructors `not_found`, `internal`, `unavailable`. `From<Error>` maps `Error::NotFound` to `404` and everything else to `500`. `IntoResponse`:

- picks a generic public message for server errors (detail goes only to logs),
- logs `503` at `warn` (expected under load) and other `5xx` at `error`, `4xx` at `warn`,
- always adds `Cache-Control: no-store`, plus `Retry-After: 1` on `503`.

## CORS

`cors_layer(&CorsConfig)` converts the validated strings into `tower-http` types (`AllowOrigin`, `AllowMethods`, `AllowHeaders`, `ExposeHeaders`), applies `max_age` and `allow_credentials`, and returns `None` when disabled. Parsing failures return `Error::Configuration`, so a bad value stops startup.

## Metrics

`observability/metrics.rs`: a `Metrics` struct of atomics: a `requests[route][status]` matrix, per-route histogram buckets and duration sums, and scalar counters (bytes, shed, queue timeouts, three abort reasons, an in-flight gauge). `Metrics::new(&ROUTES)` takes the route templates from `http/router.rs`, which defines each template once and uses the same constant to register the handler; two extra slots cover `unmatched` (no route matched) and `other`. `STATUSES` lists the tracked codes and anything else folds into `other`, so label cardinality is fixed. `Metrics::route_index` maps Axum's `MatchedPath` to an index. `render` writes Prometheus text.

**Contributing (adding a route):** add it to `router`, add a template constant and list it in `ROUTES` in `http/router.rs` (metrics pick it up automatically; bump the array length), decide whether it should bypass `shed_load`, and add a test that drives it with `oneshot`. Handlers should return `HttpResult<Response>` and never format internal detail into a response.
