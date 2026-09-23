#!/usr/bin/env sh
# Generates a ~2-minute asset for manually testing playback: seeking, scrubbing, and watching a
# player behave over more than a few seconds. Unlike generate.sh and generate-variants.sh, this
# is not a test fixture — nothing in the test suite reads it, it is not deterministic byte-for-
# byte across FFmpeg versions, and it is not committed (see tests/fixtures/generated/ in
# .gitignore). It is stream-copied from the committed 3-second fixture, the same technique
# benches/budgets.rs uses for its 60-minute benchmark asset, so it costs a few seconds and a few
# megabytes, not a real encode.
set -eu

fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
source="$fixture_dir/h264-aac.mp4"
out_dir="$fixture_dir/generated"
out="$out_dir/long.mp4"

mkdir -p "$out_dir"
echo "generating $out (~2 minutes, stream copy)..."
ffmpeg \
    -hide_banner \
    -loglevel error \
    -y \
    -stream_loop 39 \
    -i "$source" \
    -c copy \
    -use_editlist 0 \
    "$out"
echo "done: $out"
