# segmentor vs nginx-vod-module

A head-to-head benchmark: both servers serve the same 60-minute file, one at a time, on the
same pinned cores, under the same k6 load.

```sh
make bench-compare    # from the repository root; needs Docker
SERVER_CPUS=0 LOAD_CPUS=1-7 make bench-compare   # servers on one core, so the server is the limit
```

Methodology, what is made equal between the two, results, and limits are in
[docs/benchmarks.md](../../docs/benchmarks.md#segmentor-vs-nginx-vod-module). Each run writes
`results/<timestamp>/report.md` with the raw k6 summaries and resource samples beside it.

| File | Purpose |
| --- | --- |
| `run.sh` | Builds both images and runs the cold-start, manifest, segment, and correctness checks |
| `docker-compose.yml` | segmentor, nginx-vod-module with and without its response cache (`nginx`, `nginx-cached`), and k6, with CPU pinning (`SERVER_CPUS`, `LOAD_CPUS`) |
| `segmentor.toml`, `nginx-vod-module/nginx.conf` | Equivalent configurations; `segmentor-muxed.toml` turns on muxed HLS audio, as nginx-vod-module serves by default; `nginx-vod-module/response-cache-on.conf` is mounted over the empty `response-cache.conf` for `nginx-cached` |
| `nginx-vod-module/Dockerfile` | nginx with nginx-vod-module from the upstream release tarballs (AGPL-3.0; built and run separately, never copied into segmentor) |
| `k6/load.js` | Discovers each server's playlists and segments from its master playlist, then loads them |
| `crawl.py`, `sample.py`, `summarize.py` | Presentation check, CPU and memory sampling, report |
