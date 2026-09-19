# syntax=docker/dockerfile:1.7

# Multi-stage build tuned for Rust:
#   chef     - installs cargo-chef once (cached across builds)
#   planner  - reduces the source tree to a dependency "recipe"
#   builder  - compiles dependencies from the recipe (cached until Cargo.toml/lock change),
#              then compiles the application
#   runtime  - distroless glibc image with only the binary, running as non-root
#
# Build:  docker build -t vod-module-rs .
# Pin the bases for reproducible builds, e.g.
#   --build-arg RUST_IMAGE=rust:<version>-slim-bookworm@sha256:<digest>
#   --build-arg RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:<digest>
ARG RUST_IMAGE=rust:1-slim-bookworm
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot

FROM ${RUST_IMAGE} AS chef
# `aws-lc-sys` (the TLS provider behind reqwest) compiles C code and wants cmake.
RUN apt-get update \
 && apt-get install --yes --no-install-recommends build-essential cmake \
 && rm -rf /var/lib/apt/lists/*
# The cargo-chef binary is cached in this layer; the toolchain comes from the base image, so
# rust-toolchain.toml is excluded by .dockerignore and rustup never downloads components.
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# Dependencies first: this layer is reused until the dependency set changes.
COPY --from=planner /src/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo chef cook --release --locked --recipe-path recipe.json
# Then the application. The [profile.release] table in Cargo.toml sets thin LTO, one codegen
# unit, and stripped debug info.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --release --locked --bin vod-module-rs \
 && install -D target/release/vod-module-rs /out/vod-module-rs

FROM ${RUNTIME_IMAGE} AS runtime
ARG VERSION=dev
ARG REVISION=unknown
LABEL org.opencontainers.image.title="vod-module-rs" \
      org.opencontainers.image.description="On-demand HLS and DASH origin for MP4 files" \
      org.opencontainers.image.source="https://github.com/includeamin/vod-module-rs" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

COPY --from=builder /out/vod-module-rs /usr/local/bin/vod-module-rs

# TLS roots come from the distroless image's CA bundle, so https mappers and origins verify.
# Runtime contract:
#   /etc/vod/vod.toml  configuration (mount read-only); set server.listen = "0.0.0.0:3000"
#   /srv/vod           media root named by storage.media_root (mount read-only)
# The image has no shell and no writable paths the service needs, so it runs with
# `--read-only` and `--cap-drop=ALL`. Logs go to stdout as JSON.
EXPOSE 3000
USER nonroot:nonroot

# Exec form keeps the binary as PID 1 so it receives SIGTERM directly and can drain
# (see server.shutdown_delay_ms and server.shutdown_grace_ms). Give the orchestrator a stop
# timeout above their sum, for example `docker stop --time 45`.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/vod-module-rs"]
CMD ["serve", "--config", "/etc/vod/vod.toml"]

# No HEALTHCHECK: the image has no shell or curl. Probe GET /health (liveness) and GET /ready
# (readiness) from the orchestrator instead.
