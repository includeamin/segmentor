# Runtime support

The modules that everything else depends on: configuration, errors, logging, metrics, and the command-line entry point.

## `config/` : configuration

Files: `mod.rs` (`Config`, parsing, asset resolution, server, storage, packaging, and asset sections, tests), `limits.rs` (`LimitsConfig`), `cors.rs` (`CorsConfig`), `logging.rs` (`LoggingConfig`, `LogLevel`, `LogFormat`), `resolver.rs` (resolver choice, mapper client, registry, and remote-media settings, plus the redacting `Secret`).

`Config::load(path)` reads a TOML file and calls `Config::parse(contents, config_directory)`, which deserializes into the private `RawConfig`, validates, and returns the public `Config`. The split exists because the file format (relative paths, optional sections) differs from what the rest of the program wants (canonical absolute paths, defaults applied).

Every section uses `#[serde(deny_unknown_fields)]`, so a misspelled key is an error rather than silently ignored.

| Section | Type | Notes |
| --- | --- | --- |
| `[server]` | `ServerConfig` | `listen`, `shutdown_delay_ms` (default 0), `shutdown_grace_ms` (default 30,000, must be greater than zero) |
| `[storage]` | `StorageConfig` | `media_root`, resolved against the config file's directory if relative |
| `[packaging]` | `PackagingConfig` | `segment_duration_ms` (default 6000, must be greater than zero) |
| `[logging]` | `LoggingConfig` | `level`, `format` (`json` or `compact`), `buffer_capacity` |
| `[limits]` | `LimitsConfig` | Resource limits, all greater than zero (table below) |
| `[cors]` | `CorsConfig` | See [TDD 0003](../technical-design/0003-production-grade-http-api.md#cors) |
| `[assets.<id>]` | `AssetConfig` | `path` relative to `media_root`; static resolver only |
| `[resolver]`, `[resolver.http]` | `ResolverSettings`, `MapperConfig` | `type = "static"` (default) or `"http"`; the two forms are mutually exclusive with `[assets]` and are validated together |
| `[registry]` | `RegistryConfig` | Resolution cache size, load queue timeout, and preload |
| `[remote_media]` | `RemoteMediaConfig` | Host allow-list, address policy, and limits for `http` locations |

**Asset resolution.** The media root is canonicalized and must be a directory. Each asset ID must be ASCII letters, digits, `-`, or `_` (`validate_asset_id`) and each path must be relative. The path is joined to the root and canonicalized (which resolves symlinks), and the result must still start with the root and be a regular file. This blocks `..` and symlink escapes. At least one asset is required, and the count is capped by `limits.max_assets`.

**Limits**

| Limit | Default | Enforced in |
| --- | ---: | --- |
| `max_assets` | 1,000 | `Config::parse` |
| `max_source_bytes` | 1 TiB | `mp4::parse` |
| `max_metadata_bytes` | 64 MiB | `mp4::parser::find_moov` |
| `max_tracks` | 8 | `mp4::parse` |
| `max_samples_per_track` | 2,000,000 | `mp4::parser::parse_samples` |
| `max_samples_per_segment` | 100,000 | `segment::plan` |
| `max_segment_bytes` | 64 MiB | `fmp4::prepare_media_segment` |
| `max_segment_jobs` | 2 per CPU, max 32 | `AppState.segment_jobs` |
| `segment_queue_timeout_ms` | 2,000 | `acquire_segment_permit` |
| `stream_chunk_bytes` | 256 KiB | `StreamJob` |
| `max_request_header_bytes` | 16 KiB | `enforce_header_limit` |
| `request_timeout_ms` | 30,000 | `TimeoutLayer` |
| `max_startup_parses` | 4 | Concurrent asset loads in the registry (also bounds startup preload) |
| `max_concurrent_requests` | 10,000 | `shed_load` |
| `response_idle_timeout_ms` | 30,000 | `StreamJob::send` |
| `max_index_bytes` | 4 GiB | `AppState::load` |
| `max_connections` | 10,000 | `http/server.rs` accept loop |
| `header_read_timeout_ms` | 10,000 | hyper HTTP/1 header read, via `http/server.rs` |

`LimitsConfig::validate` rejects a zero in any of them.

**Design note:** `CorsConfig` stores plain strings and is validated logically here (non-empty lists, no `*` mixed with origins, no credentials with wildcards, origin shape) while the parsing into HTTP types happens in `http::cors_layer`.

**Adding an option:** add the field to the raw struct with a default, validate it in `parse`, expose it on `Config` if needed, document it in `vod.example.toml`, and add a test in the `config` tests using the `parse_with` helper.

## `error.rs` : the error type

One `Error` enum built with `thiserror`, and a `Result<T>` alias.

| Variant | Meaning | HTTP mapping |
| --- | --- | --- |
| `InvalidRange` | A byte range fell outside a source | `500` |
| `InvalidMedia(String)` | The file is malformed or inconsistent | `500` at request time, startup failure at load |
| `Unsupported(String)` | Valid but unsupported media (codec, edit list, encryption, ...) | Startup failure at load |
| `NotFound(&'static str)` | A track or segment does not exist | `404` |
| `Upstream(String)` | A remote origin or mapper misbehaved or was refused | `502` |
| `UpstreamUnavailable(String)` | A remote origin timed out or is overloaded (retryable) | `503` |
| `Io`, `Mp4`, `Toml` | Wrapped library errors | `500` |
| `Configuration(String)` | Invalid configuration | Startup failure |
| `Logging(String)` | Logger initialization failure | Startup failure |

Use `InvalidMedia` for "the file is broken" and `Unsupported` for "the file is fine but we do not handle it". Use `NotFound` only for things a client can legitimately ask for and not find.

## `observability/logging.rs` : non-blocking logs

`logging::init(&LoggingConfig)` returns a `LoggingGuard` that must stay alive for the process lifetime.

- Logs go through `tracing-appender`'s non-blocking writer with `.lossy(true)` and `buffered_lines_limit(buffer_capacity)`. A dedicated writer thread does the actual stdout writes. When the queue is full, new lines are **dropped** instead of blocking request tasks.
- The subscriber is JSON (flattened fields, current span included) or compact text, filtered by the configured level.
- A monitor thread wakes every 10 seconds, copies the appender's dropped-line count into a static atomic (read by `/metrics` through `dropped_lines()`), and prints a warning to stderr if it grew. It writes to stderr directly because the log queue is the thing that is saturated.
- Dropping the guard stops the monitor and flushes the writer.

Level policy: `info` for lifecycle and asset loads, `debug` for per-request events, `warn` for rejected requests, `error` for internal failures. Never log at `trace` per sample or log media payload.

## `observability/metrics.rs`

Described with the HTTP layer in [HTTP server](http-server.md#metrics).

## `main.rs`, `lib.rs`, and `cli/` : entry point and CLI

`main.rs` is ten lines: a Tokio `#[tokio::main]` that calls `segmentor::run()` and returns its `ExitCode`. `lib.rs` declares the modules, defines `APP_NAME`, and implements `run()`, which calls `cli::run` with the process arguments and, on failure, prints `segmentor: <error>` and returns a failure code.

`cli::run` reads the first argument:

- none: prints `segmentor` (used by `tests/cli.rs`);
- `serve --config <file>` (`cli/serve.rs`): `Config::load`, `logging::init`, then `http::serve`;
- `package --input <mp4> --output <dir> [--segment-duration-ms N]` (`cli/package.rs`).

Argument parsing is hand-written in `ServeOptions::parse` and `PackageOptions::parse`; unknown flags are errors.

`package` is a synchronous developer tool that runs the same pipeline as the server: open, `mp4::parse`, `segment::plan`, then for each track write `<track>-init.mp4` and `<track>-<n>.m4s` using `fmp4::write_init_segment` and `fmp4::write_media_segment`, and print a summary. `tests/package.rs` runs it twice and asserts identical bytes (determinism).

`fuzzing.rs` exposes `exercise_media_pipeline(path)` and `max_input_bytes()` (hidden from rustdoc) so the fuzz crate can drive parse, plan, init writing, and fragment preparation without including source files by path.
