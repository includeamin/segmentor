# Architecture

`vod-module-rs` is a binary application. Its entry point remains small while provider, catalog, playback, and runtime requirements are defined.

New capabilities should be introduced as focused modules under `src/`, keeping orchestration in `main.rs` and domain behavior in dedicated modules. Application behavior belongs in process-level integration tests under `tests/`; implementation details can use unit tests beside their source. Document modules and important internal interfaces with rustdoc comments and executable examples where practical.

The initial service architecture is described in [On-demand MP4 packaging core](technical-design/0001-on-demand-mp4-packaging-core.md). Durable architectural choices are tracked in the [ADR index](adr/README.md).
