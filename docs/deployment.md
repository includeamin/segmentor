# Deploying

This page is about getting segmentor running: which way to install it, and ready-made files for Docker, Kubernetes, and systemd. What the settings mean, and what to watch once it runs, is in [Operating the origin](operations.md).

## Choosing how to run it

| | Use it when | Files |
| --- | --- | --- |
| **Container** | You run services under Docker, Compose, or Kubernetes. This is the way most people should run it. | [`deploy/docker-compose.yml`](https://github.com/includeamin/segmentor/blob/main/deploy/docker-compose.yml), [`deploy/kubernetes/`](https://github.com/includeamin/segmentor/tree/main/deploy/kubernetes) |
| **Release binary** | You run on a plain Linux host under systemd, or want no container runtime. | [`install.sh`](https://github.com/includeamin/segmentor/blob/main/install.sh), [`deploy/systemd/`](https://github.com/includeamin/segmentor/tree/main/deploy/systemd) |
| **From source** | You are developing it, or need a platform that has no release build. | `cargo install --git https://github.com/includeamin/segmentor` |

The container is the recommended route because the image already carries what the binary needs: it is built on Debian to match its minimal, non-root runtime, so it does not depend on which glibc your host has. Release binaries are dynamically linked against glibc, so they will not run on Alpine or very old distributions.

## Before you expose it

segmentor has **no TLS and no authentication**. Put a reverse proxy or a CDN in front of it that provides both, and let that layer cache media:

- **TLS.** For a single host, [Caddy](https://caddyserver.com) does it in two lines, and gets certificates on its own:

  ```text
  video.example.com {
      reverse_proxy 127.0.0.1:3000
  }
  ```

  nginx, Envoy, and a cloud load balancer work equally well. Use HTTP/2 or HTTP/3 to viewers.
- **Caching.** Init and media segment URLs carry a `v` query parameter that changes whenever the media does, and are served `Cache-Control: public, max-age=31536000, immutable`. Playlists and manifests are served with a short `max-age` (60 s). So a CDN should **include the query string in its cache key**, and can then cache segments indefinitely without ever serving stale bytes.
- **Access control.** If viewers must be authorised, do it at the proxy or CDN, for example with signed URLs or tokens. The origin should be reachable only from that layer.
- **Per-client limits.** segmentor limits total connections and concurrent requests, but not per client address. Rate-limit at the proxy if clients are untrusted.

## Container

The image is `ghcr.io/includeamin/segmentor`, for `linux/amd64` and `linux/arm64`. It is tagged `X.Y.Z` and `X.Y` for each release, and `latest` or `preview` according to the release channel. In production, pin it by digest, since a tag can move, and [verify it](releasing.md#verifying-a-release).

The image expects two mounts:

| Path | Holds |
| --- | --- |
| `/etc/vod/vod.toml` | The configuration, read-only. Set `server.listen = "0.0.0.0:3000"`, since inside a container `127.0.0.1` is unreachable. |
| `/srv/vod` | The media root that `storage.media_root` names, read-only. |

It runs as a non-root user, needs no writable path, and works with `--read-only` and `--cap-drop=ALL`. Logs go to standard output as JSON.

### Docker Compose

[`deploy/docker-compose.yml`](https://github.com/includeamin/segmentor/blob/main/deploy/docker-compose.yml) runs it with the [example configuration](https://github.com/includeamin/segmentor/blob/main/deploy/config/vod.toml) and a `media/` directory next to `deploy/`:

```sh
mkdir -p media && cp /path/to/some-video.mp4 media/sample.mp4
docker compose -f deploy/docker-compose.yml up
ffplay http://127.0.0.1:3000/hls/sample/master.m3u8
```

Its `stop_grace_period` is longer than the configuration's `shutdown_delay_ms` plus `shutdown_grace_ms`, so `docker compose stop` drains streams instead of cutting them. The image has no shell or `curl`, so there is no container health check; probe `/health` and `/ready` from outside.

### Kubernetes

[`deploy/kubernetes/segmentor.yaml`](https://github.com/includeamin/segmentor/blob/main/deploy/kubernetes/segmentor.yaml) is a ConfigMap, a Deployment, a Service, and a PodDisruptionBudget, validated against the Kubernetes 1.30 API schemas. Before you apply it:

- **Media.** It mounts a PersistentVolumeClaim named `segmentor-media`, which you create. Or remove the volume and the `[assets.*]` table and resolve assets through a [mapper](mapper-api.md), which is the usual choice at scale.
- **Probes.** `/health` is liveness. `/ready` is readiness and also the startup probe, and it turns `503` when shutdown begins, which is what takes a terminating pod out of the Service before its connections close. `terminationGracePeriodSeconds` is longer than the configured drain time.
- **Memory.** The memory limit must exceed `limits.max_index_bytes` (512 MiB in the example) plus headroom for segments being streamed. The sample index costs about 40 bytes per sample.
- **Ingress.** Put an Ingress or CDN with TLS in front of the Service.

## Release binary and systemd

### Install the binary

```sh
curl -fsSL https://raw.githubusercontent.com/includeamin/segmentor/main/install.sh | sh
```

Read the script first; it is short. It downloads one release archive and its checksum from GitHub, verifies the checksum, and copies one file into `~/.local/bin` (or `/usr/local/bin` when run as root). It never uses `sudo`. When the `gh` tool is installed it also checks the archive's build provenance, as a warning by default, or as a requirement with `--require-attestation`. Useful options:

```sh
sh install.sh --version v0.4.0 --prefix /usr/local/bin    # pin a release, choose the directory
sh install.sh --dry-run                                   # show what it would do
```

It supports Linux on x86-64 and arm64 and says so plainly anywhere else. To do the same by hand, download the archive from the [releases page](https://github.com/includeamin/segmentor/releases), then follow [Verifying a release](releasing.md#verifying-a-release).

### Run it as a service

[`deploy/systemd/segmentor.service`](https://github.com/includeamin/segmentor/blob/main/deploy/systemd/segmentor.service) runs the binary as an unprivileged `segmentor` user with the service sandboxed: no capabilities, a read-only filesystem apart from the media path, private `/tmp` and devices, only network sockets, and no way to gain privileges. `systemd-analyze security` rates the exposure `OK`.

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin segmentor
sudo install -m 0755 segmentor /usr/local/bin/segmentor
sudo install -d /etc/segmentor && sudo cp vod.toml /etc/segmentor/vod.toml
sudo cp deploy/systemd/segmentor.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now segmentor
journalctl -u segmentor -f
```

The unit lets the service read `/srv/vod`; if your `storage.media_root` is somewhere else, change `ReadOnlyPaths`. `TimeoutStopSec` is longer than the configured drain time, and `LimitNOFILE` is raised above the default connection limit.

## Upgrading and rolling back

- **Replace and restart.** Swap the image tag or the binary, and restart. The service drains in-flight streams first, within `shutdown_grace_ms`.
- **Roll one instance at a time** behind a load balancer, so `/ready` takes each out of rotation as it stops.
- **Expect new URLs.** The `v` in media URLs includes a format revision that changes when a release changes the bytes it serves for an unchanged file. After such an upgrade a CDN misses once per segment and then warms again, and players fetch the playlist to get the new URLs. It never serves old bytes under a new version, or the reverse.
- **Roll back the same way.** The previous image or binary serves its own URLs, so a rollback is safe and needs no cache purge.
