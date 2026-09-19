# Code organization

A review of the source layout and the reorganization plan. Steps 1 to 3 are **done**; steps 4 to 6 remain proposals.

## Current layout

```text
src/
  main.rs            10 lines: calls vod_module_rs::run()
  lib.rs             module declarations, run(), APP_NAME
  fuzzing.rs         hidden entry points for the fuzz target
  benchmarking.rs    hidden entry points for benches/ (real asset loader and server)
  cli/               mod.rs (dispatch), serve.rs, package.rs
  config/            mod.rs (Config, parsing, tests), limits.rs, cors.rs, logging.rs
  http/              mod.rs (serve), state.rs, router.rs, middleware.rs, error.rs,
                     cors.rs, range.rs, validators.rs, server.rs, shutdown.rs, stream.rs,
                     handlers/{mod,health,playlist,media}.rs, tests.rs
  asset.rs           PackagedAsset: source, index, plan, init segments, rendered text
  resolver/          mod.rs (types), catalog.rs (static), mapper.rs (client), policy.rs (trust rules)
  registry/          mod.rs (single flight, caches), cache.rs (byte-weighted LRU), opener.rs, tests.rs
  protocol/          mod.rs, presentation.rs (read-only view), hls.rs, dash.rs
  observability/     mod.rs, logging.rs, metrics.rs
  error.rs
  source/            mod.rs, local.rs, http.rs (remote), sparse.rs (metadata regions)
  media/  mp4/  segment/  fmp4/     one focused file each (mod.rs is only re-exports)
  testutil.rs        mock mapper and origin servers (tests only)
tests/               cli.rs, package.rs, fixtures/
fuzz/                separate crate that depends on the library
```

## Status of the reorganization

| Step | State |
| --- | --- |
| 1. `lib.rs` plus a thin `main.rs`; fuzz depends on the library | Done. The public surface is `run()` and the hidden `fuzzing` module; everything else stays `pub(crate)` |
| 2. `package` command moved into `cli/` | Done |
| 3. `http.rs` split into `http/` | Done. Largest production file is about 200 lines |
| 4. `config.rs` into `config/` | Done. Limits, CORS, and logging settings each have a file |
| 5. `hls`/`dash` into `protocol/` with a read-only view | Done. Renderers take a `Presentation` (tracks, plan, version) and no longer depend on `asset` |
| 6. `logging`/`metrics` into `observability/` | Done. The router owns the route table and passes it to `Metrics::new` |

All six steps were behavior-preserving and `make ci` stayed green. The test count went from 59 to 61 because the moves added two tests: a renderer test that a missing track is `NotFound`, and a check that route templates are unique.

Notes on how the result differs from the original proposal:

- `ETag` and `If-None-Match` handling lives in `http/validators.rs`, separate from `range.rs`.
- The router tests are one `http/tests.rs` file rather than spread across modules, because they drive the whole router.
- **Route table.** `http/router.rs` defines each route template once as a constant and builds `ROUTES` from them. The same constants register the handlers and label the metrics, so adding a route can no longer leave metrics out of date. `Metrics::new(&ROUTES)` sizes its counters from that list, with two extra slots for `unmatched` and `other`.
- **Renderer independence.** `PackagedAsset::load` builds a `Presentation` from the index, plan, and version, renders all text, and only then constructs the asset. That removed the earlier two-phase construction where an asset was created with empty text and then filled in. Bandwidth calculation moved from `PackagedAsset` into `Presentation`.
- Renderer tests build a `Presentation` from the fixture directly and no longer load a full asset.

## Assessment

What works well:

- **The media pipeline is cleanly layered.** `source`, `media`, `mp4`, `segment`, and `fmp4` have one job each, no HTTP knowledge, and a one-directional dependency order. New contributors can work in one without reading the others.
- **Files are small everywhere except one.** Apart from `http.rs`, every file is a few hundred lines with a single responsibility.
- **Tests sit next to the code** and the tests directory is small and purposeful.

What will not scale:

1. **`http.rs` mixes about ten concerns in one 1,600-line file**: state loading, router and layer wiring, four middlewares, playlist handlers, segment handlers, the streaming task, range and ETag parsing, CORS construction, the error type, shutdown, and a long test module. Any HTTP change touches it, merge conflicts are likely, and reviewers cannot see boundaries.
2. **The crate is binary-only.** There is no `lib.rs`. Consequences:
   - the fuzz target must include source files with `#[path]`, which is brittle (it forces `config.rs` to stay free of HTTP dependencies, for example);
   - benchmarks (needed for the pending performance budgets) and integration tests cannot import the packaging code;
   - `pub(crate)` everywhere means no enforced API boundary.
3. **`main.rs` holds application code**: the `package` command is real logic (about 80 lines) living in the entry point.
4. **Cross-cutting infrastructure is at the top level.** `metrics.rs` and `logging.rs` are observability, and `metrics.rs` hard-codes the HTTP route list, coupling it to `http.rs`.
5. **`hls`/`dash` and `asset` reference each other**, a small cycle: `asset` calls the renderers, and the renderers read `PackagedAsset`.
6. **Upcoming work will worsen these.** [TDD 0002](../technical-design/0002-asset-map-interface.md) adds a registry, a resolver, a mapper client, an HTTP media source, and more config. Without a structure for it, all of it lands in `http.rs`, `config.rs`, and `asset.rs`.

Verdict (written before steps 1 to 3): manageable, but worth reorganizing before the mapper work. The HTTP layer, crate boundary, and CLI were the problem areas and are now addressed. The assessment below is kept as the rationale.

## Recommended structure

```text
src/
  main.rs                  thin: parse args, call into the library
  lib.rs                   module declarations; small, documented public surface
  cli/
    mod.rs                 command dispatch
    serve.rs               `serve` options and startup
    package.rs             `package` command (moved from main.rs)
  config/
    mod.rs                 Config, load/parse
    limits.rs              LimitsConfig
    cors.rs                CorsConfig
    logging.rs             LoggingConfig
  error.rs
  media/                   MediaIndex, Track, Sample           (unchanged)
  source/                  MediaSource, local                  (unchanged; remote source lands here)
  mp4/                     parser                              (unchanged)
  segment/                 planner                             (unchanged)
  fmp4/                    init, fragment                      (unchanged)
  asset/
    mod.rs                 PackagedAsset
    render.rs              RenderedManifests
    bandwidth.rs           Bandwidth calculation
  protocol/
    hls.rs
    dash.rs                renderers move here, next to (not inside) asset
  http/
    mod.rs                 serve(), re-exports
    state.rs               AppState, load
    router.rs              routes and layer order
    middleware.rs          request_id, metrics, shed_load, header limit
    handlers/
      health.rs            health, ready, metrics
      playlist.rs          master, media playlist, DASH manifest
      media.rs             init_segment, media_segment
    stream.rs              StreamJob
    range.rs               ETag, If-None-Match, Range, If-Range
    cors.rs                cors_layer
    error.rs               HttpError
    shutdown.rs            shutdown_signal
  observability/
    logging.rs
    metrics.rs             route list defined by http/router.rs, passed in
tests/                     integration tests can now `use vod_module_rs::...`
benches/                   criterion benchmarks for the performance budgets
fuzz/                      depends on the library crate, no #[path] includes
```

Key points:

- **`lib.rs` plus a thin `main.rs`.** The binary becomes about ten lines. The fuzz crate and benchmarks depend on the library like any other crate, removing the `#[path]` workaround and the "no HTTP crates in config" constraint.
- **`http/` split by concern.** Each file has one reason to change, tests move beside the code they cover (for example range tests into `range.rs`), and the large router test module becomes a `tests/http_*.rs` integration suite that can share a small helper module.
- **`protocol/` breaks the asset/renderer cycle.** Renderers take a read-only view (tracks, segments, version, bandwidth) and return text; `asset` calls them; `protocol` no longer depends on `asset`'s internals.
- **`observability/`** groups logging and metrics, and metrics stops owning the route table.
- **Visibility.** Make the library's public surface deliberate: `pub` for what the binary, benches, and fuzz need, `pub(crate)` for the rest.

## Suggested order (all steps done)

Each step is behavior-preserving and keeps `make ci` green, so it can ship as its own small change.

1. Add `lib.rs`, make `main.rs` thin, point fuzz at the library. (Unblocks benchmarks.)
2. Move the `package` command into `cli/`.
3. Split `http.rs` into `http/` (start with pure pieces: `range.rs`, `error.rs`, `cors.rs`, `shutdown.rs`; then `stream.rs`; then handlers and middleware).
4. Split `config.rs` into `config/`.
5. Move `hls`/`dash` to `protocol/` and pass a read-only view.
6. Group `logging`/`metrics` under `observability/`.

This was done **before** the mapper work so the registry and resolver can arrive as new modules (`registry/`, `resolver/`) instead of edits to existing large files.

## What not to do

- Do not create a Cargo workspace of many crates yet. The code is one deployable, compile times are small, and cross-crate visibility would add friction without a second consumer.
- Do not introduce traits for their own sake. The media pipeline uses concrete types on purpose; the one trait that matters (`MediaSource`, becoming an enum in TDD 0002) should stay minimal.
- Do not reorganize and change behavior in the same change.
