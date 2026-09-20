# Operating the origin

This guide covers what an operator needs to run `segmentor serve` behind a load balancer or CDN. Every setting named here is documented in `vod.example.toml`.

## Deployment shape

The service is an HTTP origin. It does not terminate TLS, authenticate viewers, or rate-limit clients. Run it behind a reverse proxy or CDN that does, and cache media responses there: segment URLs are content-versioned and served `immutable`.

Two protections are built in because a proxy cannot fully provide them:

- `limits.max_concurrent_requests` answers `503` with `Retry-After: 1` once that many handlers are running. `/health`, `/ready`, and `/metrics` are exempt.
- `limits.response_idle_timeout_ms` closes a media response whose client stops reading, and a job slot is held only while a source read is in flight. A stalled client therefore cannot exhaust `limits.max_segment_jobs`.

- `limits.header_read_timeout_ms` (default 10 s) closes a connection that has not delivered a complete request header block in that time, which stops slow-header (slowloris) clients. It also bounds how long an idle keep-alive connection waits for its next request.
- `limits.max_connections` (default 10,000) closes connections beyond the cap immediately at accept. Raise the process file-descriptor limit (`ulimit -n`, or `LimitNOFILE` under systemd) above this number plus headroom for media file handles; otherwise accept fails with `EMFILE` before the cap applies (the accept loop then logs `accept_failed` and backs off for a second).

The service still does not limit connections per client address. Do that on the proxy or load balancer if clients are untrusted.

## Probes

| Path | Meaning | Use for |
| --- | --- | --- |
| `/health` | The process is running | Liveness probe |
| `/ready` | `200` while serving, `503` once shutdown begins | Readiness probe and load-balancer health check |
| `/metrics` | Prometheus text | Scraping |

## Shutdown

The service handles `SIGTERM` and `SIGINT`. On either signal it:

1. flips `/ready` to `503`;
2. keeps accepting connections for `server.shutdown_delay_ms`, so a load balancer can notice and drain;
3. stops accepting new connections and lets in-flight responses finish;
4. exits after at most `server.shutdown_grace_ms` more, closing any stream still open.

Set the orchestrator's termination grace period to at least `shutdown_delay_ms + shutdown_grace_ms` plus a few seconds. In Kubernetes, a `shutdown_delay_ms` of 5000 to 10000 typically covers endpoint propagation.

## CORS

Browser players need CORS. The `[cors]` table configures it, and `enabled = false` turns it off when the proxy or CDN adds the headers instead. Restrict `allowed_origins` to your player origins in production. `allow_credentials = true` requires explicit origins, methods, and headers; the configuration is rejected at startup otherwise. The defaults expose `Content-Length`, `Content-Range`, `Accept-Ranges`, `ETag`, and `X-Request-Id` to scripts and allow the `Range`, `If-None-Match`, and `If-Range` request headers.

## Request tracing

Every response carries `X-Request-Id`. A caller-supplied ID (1 to 128 characters from letters, digits, `-`, `_`, `.`) is kept, and anything else is replaced. The ID is attached to the request span, so it appears on every log line for that request.

## Metrics

| Metric | Type | Notes |
| --- | --- | --- |
| `vod_http_requests_total{route,status}` | counter | `route` is the route template, so cardinality is fixed |
| `vod_http_request_duration_seconds{route}` | histogram | Time to response headers, not body transfer |
| `vod_http_requests_in_flight` | gauge | |
| `vod_http_response_bytes_total` | counter | Bytes handed to the response stream |
| `vod_source_read_bytes_total` | counter | Compare with response bytes to check read amplification |
| `vod_http_requests_shed_total` | counter | Requests refused by `max_concurrent_requests` |
| `vod_http_connections_open` | gauge | Open TCP connections |
| `vod_http_connections_rejected_total` | counter | Connections closed at accept because of `max_connections` |
| `vod_segment_queue_timeouts_total` | counter | Waits that exceeded `segment_queue_timeout_ms` |
| `vod_segment_stream_aborts_{idle,client,error}_total` | counter | Streams that ended early, by cause |
| `vod_log_dropped_lines_total` | counter | Log records dropped by the lossy queue |

Alert on a rising `vod_segment_queue_timeouts_total` or `vod_http_requests_shed_total` (undersized limits or overload) and on a non-zero rate of `stream_aborts_error`.

## Memory

Each loaded asset keeps its full sample index in memory, about 40 bytes per sample. `limits.max_index_bytes` (default 4 GiB) rejects a catalog whose combined indexes exceed it, and startup fails with the measured size. Size the container's memory limit above that budget plus headroom for in-flight segment reads (`stream_chunk_bytes` times `max_segment_jobs`).

## Resolving assets from a mapper

By default the catalog is the `[assets.*]` tables and every asset is loaded before the server accepts traffic. To resolve assets from an external service instead, replace those tables with a resolver (see the commented example in `vod.example.toml` and the [Mapper API reference](mapper-api.md)):

```toml
[resolver]
type = "http"

[resolver.http]
base_url = "https://mapper.internal.example.net"
bearer_token_env = "VOD_MAPPER_TOKEN"
```

Behavior worth knowing before you run it:

- **Assets load on first request.** The first viewer of an asset pays the resolve, open, and parse cost (about 100 ms for a one-hour file; more for a remote object). Later requests are served from memory. Warm popular assets with a request after deployment if that matters.
- **Memory is bounded by bytes.** Loaded assets are kept in a least-recently-used cache limited by `limits.max_index_bytes` (about 40 bytes per sample). An evicted asset reloads transparently.
- **Mapper answers are cached** for their TTL (clamped by `min_ttl_ms` and `max_ttl_ms`), revalidated with `If-None-Match`, and a missing asset is remembered for `negative_ttl_ms`.
- **A mapper outage does not stop playback of known assets.** An expired answer is served for up to `stale_if_error_ms` while the mapper is down. A location with an `expires_at` (a signed URL) is never served past that time. Unknown assets return `503` until the mapper recovers.
- **A changed asset switches at once.** When the mapper returns a new version, the old one is dropped. Players holding old versioned URLs get `404` and refetch the playlist.
- **Set `readiness_probe_interval_ms`** if you want `/ready` to report `503` while the mapper is unreachable, so a load balancer can hold new traffic. `/health` is unaffected.
- **Startup does not depend on the mapper.** The process starts even if the mapper is down.

### Remote media

A mapper can return `http` locations, in which case the server reads the media from that origin with ranged requests. It reads only the headers and `moov` metadata to load an asset, then fetches segment bytes as they are requested. Because a mapper controls where the server connects, `[remote_media]` is a security boundary:

- `allowed_hosts` must list every origin host; an empty list refuses all remote locations.
- Locations must be `https` (`allow_insecure_http` is for development), carry no credentials, and are never redirected.
- Names that resolve to loopback, private, link-local, shared, or multicast addresses are refused unless `allow_private_addresses = true`. Leave it off in production so a mapper cannot point the server at internal services.
- The origin must support `Range` and send a strong `ETag` or a `Last-Modified`; reads are conditional on it, so a replaced object fails playback instead of mixing versions.
- Signed URLs are supported: the mapper should set `expires_at` and re-sign under the same `version`. The server refreshes ahead of expiry (`resolver.http.refresh_margin_ms`), rotates the URL on a loaded asset without reloading it, and re-asks the mapper once if the origin rejects a read with `401`, `403`, or `410`. Watch `vod_location_rotations_total`.

TLS uses the operating system's trusted roots (the container image carries a CA bundle).

### Mapper and registry metrics

| Metric | Type | Notes |
| --- | --- | --- |
| `vod_resolver_requests_total{outcome}` | counter | `ok`, `unchanged`, `not_found`, `unavailable`, `rejected` |
| `vod_resolution_cache_events_total{event}` | counter | `hit`, `miss`, `revalidate`, `stale`, `negative_hit` |
| `vod_asset_loads_total{outcome}` | counter | `ok` or `failed` |
| `vod_asset_load_seconds_total` | counter | Divide by loads for the mean load time |
| `vod_location_rotations_total` | counter | Signed URLs replaced in place on loaded assets |
| `vod_registry_coalesced_waiters_total` | counter | Requests that shared another request's resolve or load |
| `vod_loaded_assets`, `vod_loaded_bytes` | gauge | What is in memory now |

Alert on a rising `unavailable` or `rejected` count (mapper trouble or a bad answer), on `stale` events (the mapper is down and old data is being served), and on `failed` loads.

## Container image

The `Dockerfile` is a four-stage build optimized for Rust:

- **Dependency caching.** `cargo-chef` compiles dependencies from a recipe derived from `Cargo.toml` and `Cargo.lock`, so a source-only change rebuilds only the application. BuildKit cache mounts keep the cargo registry between builds.
- **Reproducibility.** Builds use `--locked`. Pin `RUST_IMAGE` and `RUNTIME_IMAGE` to a version and digest for release builds (see the header of the file).
- **Small, hardened runtime.** The final stage is `distroless/cc-debian12:nonroot`: glibc only, no shell or package manager, running as a non-root user. The binary is dynamically linked against glibc rather than musl on purpose, because musl's allocator is slower under this multi-threaded workload.
- **Release profile.** `[profile.release]` uses thin LTO, one codegen unit, and stripped debug info.
- **Signals.** The entrypoint is in exec form so the service is PID 1 and receives `SIGTERM` directly.
- **Build context.** `.dockerignore` admits only the manifests and `src/`, so unrelated changes do not invalidate layers.

```sh
docker build -t segmentor --build-arg VERSION=0.1.0 --build-arg REVISION=$(git rev-parse HEAD) .
docker run --rm -p 3000:3000 --read-only --cap-drop=ALL \
  -v "$PWD/vod.toml:/etc/vod/vod.toml:ro" -v "$PWD/media:/srv/vod:ro" segmentor
```

Set `server.listen = "0.0.0.0:3000"` and `storage.media_root = "/srv/vod"` in the mounted configuration. The build stage installs `cmake` and a C toolchain because the TLS provider (`aws-lc-sys`) compiles C code. Use `docker stop --time` (or the orchestrator's termination grace period) above `shutdown_delay_ms + shutdown_grace_ms`. The image has no `HEALTHCHECK` because it contains no shell or HTTP client; probe `/health` and `/ready` from the orchestrator.
