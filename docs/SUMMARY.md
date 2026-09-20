# Summary

[Introduction](README.md)

# Design

- [Architecture](architecture.md)
- [Operating the origin](operations.md)
- [Performance budgets](benchmarks.md)
- [Protocol conformance](conformance.md)
- [Mapper API reference](mapper-api.md)
- [Releasing](releasing.md)
- [Technical design documents](technical-design/README.md)
  - [On-demand MP4 packaging core](technical-design/0001-on-demand-mp4-packaging-core.md)
  - [Asset mapper interface](technical-design/0002-asset-map-interface.md)
  - [Production-grade HTTP API](technical-design/0003-production-grade-http-api.md)
  - [Broader MP4 input support](technical-design/0004-broader-mp4-input-support.md)
  - [Technical design template](technical-design/template.md)

# Implementation

- [Implementation guide](internals/README.md)
  - [Media pipeline](internals/media-pipeline.md)
  - [Protocols and assets](internals/protocols.md)
  - [Registry and resolvers](internals/registry-and-resolvers.md)
  - [HTTP server](internals/http-server.md)
  - [Runtime support](internals/runtime-support.md)
  - [Testing](internals/testing.md)
  - [Code organization](internals/code-organization.md)

# Research

- [How nginx-vod-module works](research/nginx-vod-module.md)

# Decisions

- [Architectural decision records](adr/README.md)
  - [ADR 0001: Use fragmented MP4](adr/0001-use-fragmented-mp4-for-media-segments.md)
  - [ADR template](adr/template.md)

# Reference

- [API reference](api-reference.md)
