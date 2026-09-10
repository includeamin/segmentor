#!/usr/bin/env sh
set -eu

fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
fixture="$fixture_dir/h264-aac.mp4"
probe="$fixture_dir/h264-aac.ffprobe.json"

ffmpeg \
    -hide_banner \
    -loglevel error \
    -f lavfi \
    -i "testsrc2=size=320x180:rate=30:duration=3" \
    -f lavfi \
    -i "sine=frequency=1000:sample_rate=48000:duration=3" \
    -c:v libx264 \
    -pix_fmt yuv420p \
    -preset medium \
    -g 30 \
    -keyint_min 30 \
    -sc_threshold 0 \
    -bf 2 \
    -c:a aac \
    -profile:a aac_low \
    -b:a 96k \
    -movflags +faststart \
    -y \
    "$fixture"

ffprobe \
    -v error \
    -show_format \
    -show_streams \
    -show_packets \
    -of json \
    "$fixture" > "$probe"

printf 'Generated %s and %s\n' "$fixture" "$probe"