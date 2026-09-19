# Operating the origin

This guide covers what an operator needs to run `vod-module-rs serve` behind a load balancer or CDN. Every setting named here is documented in `vod.example.toml`.

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

## Container image

The `Dockerfile` is a four-stage build optimized for Rust:

- **Dependency caching.** `cargo-chef` compiles dependencies from a recipe derived from `Cargo.toml` and `Cargo.lock`, so a source-only change rebuilds only the application. BuildKit cache mounts keep the cargo registry between builds.
- **Reproducibility.** Builds use `--locked`. Pin `RUST_IMAGE` and `RUNTIME_IMAGE` to a version and digest for release builds (see the header of the file).
- **Small, hardened runtime.** The final stage is `distroless/cc-debian12:nonroot`: glibc only, no shell or package manager, running as a non-root user. The binary is dynamically linked against glibc rather than musl on purpose, because musl's allocator is slower under this multi-threaded workload.
- **Release profile.** `[profile.release]` uses thin LTO, one codegen unit, and stripped debug info.
- **Signals.** The entrypoint is in exec form so the service is PID 1 and receives `SIGTERM` directly.
- **Build context.** `.dockerignore` admits only the manifests and `src/`, so unrelated changes do not invalidate layers.

```sh
docker build -t vod-module-rs --build-arg VERSION=0.1.0 --build-arg REVISION=$(git rev-parse HEAD) .
docker run --rm -p 3000:3000 --read-only --cap-drop=ALL \
  -v "$PWD/vod.toml:/etc/vod/vod.toml:ro" -v "$PWD/media:/srv/vod:ro" vod-module-rs
```

Set `server.listen = "0.0.0.0:3000"` and `storage.media_root = "/srv/vod"` in the mounted configuration. Use `docker stop --time` (or the orchestrator's termination grace period) above `shutdown_delay_ms + shutdown_grace_ms`. The image has no `HEALTHCHECK` because it contains no shell or HTTP client; probe `/health` and `/ready` from the orchestrator.
