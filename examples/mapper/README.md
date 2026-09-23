# Example mapper

A minimal implementation of the [mapper API](../../docs/mapper-api.md), in Python's standard
library only. It answers `GET /v1/health` and `GET /v1/assets/{id}` from [`catalog.json`](catalog.json),
re-read on every request, so editing the catalog is visible within its 5-second TTL without a
restart.

It exists to give `../../docker-compose.dev.yml` a real mapper to resolve assets against, and to
be a concrete second reading of the wire protocol next to the code block in the docs. **It is not
meant to run in production:** no TLS, no authentication, and a TTL chosen to make editing the
catalog feel immediate rather than to be efficient at real traffic. A production mapper is
whatever already knows where your media lives — most are a few lines in an existing service, not
a new one.

## Run it on its own

```sh
cd examples/mapper
python3 mapper.py                      # serves catalog.json on :9911
curl http://127.0.0.1:9911/v1/assets/sample
```

Point segmentor at it with:

```toml
[resolver]
type = "http"

[resolver.http]
base_url = "http://127.0.0.1:9911"
allow_insecure_mapper = true   # development only; a real mapper must be reached over https
```

## Run it with segmentor and the web pages

See [`docker-compose.dev.yml`](../../docker-compose.dev.yml) at the repository root, which builds
this mapper, builds segmentor from the local source tree, and serves the [demo player](../../demo/)
and the [control panel](../../admin/) alongside them — a complete local setup with one command.

## The catalog

`catalog.json` maps asset IDs to a location and, for `sample`, sidecar subtitles — the same shape
a mapper answer's `location` and `subtitles` fields take. The paths are relative to whatever
`storage.media_root` segmentor is configured with; `docker-compose.dev.yml` mounts the project's
`tests/fixtures/` there, so the catalog points at files already committed to the repository.

`long` is the exception: at three seconds, the committed fixtures loop too fast to seek or scrub
through by hand. `make fixtures-long` generates a ~2-minute version once (stream-copied, so it
costs seconds, not a real encode) at `tests/fixtures/generated/long.mp4`, which the compose mount
picks up without a restart. It is not committed; requesting `long` before generating it fails like
any other missing file — a mapper is asked lazily, per asset, so this cannot break startup the way
a bad entry in a static `[assets.*]` catalog would.
