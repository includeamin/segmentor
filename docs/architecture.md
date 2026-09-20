# Architecture

`segmentor` is a single binary that serves HLS and DASH by repackaging MP4 files on demand. The layers are:

```text
HTTP layer (http, metrics)  ->  loaded assets (asset, hls, dash)  ->  media pipeline (source, mp4, media, segment, fmp4)
```

The media pipeline has no HTTP knowledge, the protocol renderers know nothing about transport, and the HTTP layer is the only place that deals with clients, limits, and failure modes.

- [Implementation guide](internals/README.md): module-by-module description, lifecycles, and concurrency model.
- [Code organization](internals/code-organization.md): review of the source layout and a proposed reorganization.
- Design records: [packaging core](technical-design/0001-on-demand-mp4-packaging-core.md), [asset mapper](technical-design/0002-asset-map-interface.md), [production-grade HTTP API](technical-design/0003-production-grade-http-api.md), and the [ADR index](adr/README.md).
- [Operating the origin](operations.md) for deployment.

New capabilities should be introduced as focused modules under `src/`. Application behavior belongs in process-level integration tests under `tests/`; implementation details use unit tests beside their source.
