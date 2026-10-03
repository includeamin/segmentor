#!/usr/bin/env bash
# Runs the segmentor vs nginx-vod-module comparison and writes a report.
#
#   ./run.sh                      # both servers, default settings
#   VUS=256 DURATION=60s ./run.sh
#   SERVERS=segmentor ./run.sh    # just one (segmentor, segmentor-separate, nginx, nginx-cached)
#
# For each server, one at a time on the same pinned cores:
#   1. cold start: restart, then time the first master playlist, media playlist, and segment;
#      then, in that running process, the same for assets it has not loaded yet
#   2. manifests: playlists under load
#   3. segments: init and media segments under load, sampling the server's CPU and memory
#   4. correctness: FFmpeg decodes the first 30 s of what it served, and the presentation is
#      crawled once to count bytes
# Results go to results/<timestamp>/, with report.md summarizing them.
set -euo pipefail
cd "$(dirname "$0")"

SERVERS=${SERVERS:-"segmentor segmentor-separate nginx nginx-cached"}
VUS=${VUS:-64}
DURATION=${DURATION:-30s}
COLD_REPS=${COLD_REPS:-5}
export SERVER_CPUS=${SERVER_CPUS:-0-3}
export LOAD_CPUS=${LOAD_CPUS:-4-7}
stamp=$(date -u +%Y%m%dT%H%M%SZ)
out="results/$stamp"
mkdir -p "$out"
chmod 777 "$out" # k6 runs as an unprivileged user in its container

# The same 60-minute asset the budget benchmark uses: the committed fixture stream-copied 1,200
# times, no re-encode.
if [[ ! -f media/long.mp4 ]]; then
  echo "generating media/long.mp4 (60 minutes, stream copy)..."
  ffmpeg -hide_banner -loglevel error -y -stream_loop 1199 \
    -i ../../tests/fixtures/h264-aac.mp4 -c copy -use_editlist 0 media/long.mp4
fi

# Five more names for the same file, so a running server can be asked for assets it has not
# loaded. Hard links share the page cache, so only the servers' own caches are cold.
for i in 1 2 3 4 5; do
  [[ -e media/long-$i.mp4 ]] || ln media/long.mp4 "media/long-$i.mp4"
done

echo "building images..."
docker compose build segmentor nginx >"$out/build.log" 2>&1

# nginx-cached is the same nginx-vod-module image with its response cache on.
# segmentor-separate is the segmentor image with packaging.hls_mux_audio off.
declare -A port=([segmentor]=18080 [segmentor-separate]=18083 [nginx]=18081 [nginx-cached]=18082)
declare -A internal=([segmentor]=http://segmentor:3000 [segmentor-separate]=http://segmentor-separate:3000 [nginx]=http://nginx:80 [nginx-cached]=http://nginx-cached:80)
declare -A asset=([segmentor]=long [segmentor-separate]=long [nginx]=long.mp4 [nginx-cached]=long.mp4)
declare -A master=([segmentor]=/hls/long/master.m3u8 [segmentor-separate]=/hls/long/master.m3u8 [nginx]=/hls/long.mp4/master.m3u8 [nginx-cached]=/hls/long.mp4/master.m3u8)

wait_healthy() {
  for _ in $(seq 1 100); do
    curl -fs -o /dev/null "http://127.0.0.1:$1/health" && return 0
    sleep 0.1
  done
  echo "server on :$1 did not become healthy" >&2
  return 1
}

first_lines() { # url -> first media playlist URL, then its first segment URL (relative resolved)
  python3 - "$1" <<'PY'
import sys, urllib.request
from urllib.parse import urljoin
master = sys.argv[1]
text = urllib.request.urlopen(master).read().decode()
lines = [l.strip() for l in text.splitlines()]
playlist = urljoin(master, next(lines[i + 1] for i, l in enumerate(lines) if l.startswith("#EXT-X-STREAM-INF")))
body = urllib.request.urlopen(playlist).read().decode()
segment = urljoin(playlist, next(l.strip() for l in body.splitlines() if l.strip() and not l.startswith("#")))
print(playlist)
print(segment)
PY
}

for server in $SERVERS; do
  echo "== $server"
  docker compose down --remove-orphans >/dev/null 2>&1 || true
  docker compose up -d "$server" >/dev/null
  wait_healthy "${port[$server]}"
  base="http://127.0.0.1:${port[$server]}"
  mapfile -t urls < <(first_lines "$base${master[$server]}")
  playlist_url=${urls[0]}
  segment_url=${urls[1]}

  # 1. Cold start: a fresh process each time, so nothing is parsed or cached yet.
  echo "rep,master_s,playlist_s,segment_s" >"$out/$server-cold.csv"
  for rep in $(seq 1 "$COLD_REPS"); do
    docker compose restart "$server" >/dev/null
    wait_healthy "${port[$server]}"
    t_master=$(curl -fs -o /dev/null -w '%{time_total}' "$base${master[$server]}")
    t_playlist=$(curl -fs -o /dev/null -w '%{time_total}' "$playlist_url")
    t_segment=$(curl -fs -o /dev/null -w '%{time_total}' "$segment_url")
    echo "$rep,$t_master,$t_playlist,$t_segment" >>"$out/$server-cold.csv"
  done

  # 1b. Cold asset, running process: the server is already up and has served `long`; each of
  # these names is one it has never loaded. This is the usual production case: a long-tail
  # asset's first viewer, not a restart.
  echo "rep,master_s,playlist_s,segment_s" >"$out/$server-cold-asset.csv"
  for rep in 1 2 3 4 5; do
    from="/hls/${asset[$server]}/"
    to="/hls/${asset[$server]/long/long-$rep}/"
    t_master=$(curl -fs -o /dev/null -w '%{time_total}' "$base${master[$server]/$from/$to}")
    t_playlist=$(curl -fs -o /dev/null -w '%{time_total}' "${playlist_url/$from/$to}")
    t_segment=$(curl -fs -o /dev/null -w '%{time_total}' "${segment_url/$from/$to}")
    echo "$rep,$t_master,$t_playlist,$t_segment" >>"$out/$server-cold-asset.csv"
  done

  # 2 and 3. Load, with the server's resource use sampled during the segment run.
  for scenario in manifests segments; do
    stats="$out/$server-$scenario-stats.csv"
    echo "cpu_percent,mem_bytes" >"$stats"
    container=$(docker compose ps -q "$server")
    python3 sample.py "$container" "$stats" &
    sampler=$!
    docker compose --profile load run --rm \
      -e MASTER="${internal[$server]}${master[$server]}" \
      -e SCENARIO="$scenario" -e VUS="$VUS" -e DURATION="$DURATION" \
      -e NAME="$stamp/$server-$scenario" \
      k6 run --quiet /scripts/load.js >"$out/$server-$scenario-k6.log" 2>&1
    kill "$sampler" 2>/dev/null || true
    wait "$sampler" 2>/dev/null || true
  done

  # 4. Correctness: what was measured must be a playable, comparable presentation.
  if ffmpeg -v error -t 30 -i "$base${master[$server]}" -f null - 2>"$out/$server-decode.log"; then
    echo "ok" >"$out/$server-decode-status"
  else
    echo "failed" >"$out/$server-decode-status"
  fi
  python3 crawl.py "$base${master[$server]}" >"$out/$server-crawl.json"
done
docker compose down --remove-orphans >/dev/null 2>&1 || true

python3 summarize.py "$out" | tee "$out/report.md"
