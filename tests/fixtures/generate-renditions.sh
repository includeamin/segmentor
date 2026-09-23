#!/usr/bin/env sh
# Fixtures for adaptive renditions (TDD 0006 §3): several files served as one asset. Kept apart
# from generate.sh and generate-variants.sh, which are each one source file.
set -eu

fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

video="testsrc2=size=640x360:rate=30:duration=3"
video_small="testsrc2=size=320x180:rate=30:duration=3"
tone_a="sine=frequency=1000:sample_rate=48000:duration=3"
tone_b="sine=frequency=500:sample_rate=48000:duration=3"

ffmpeg_quiet() {
    ffmpeg -hide_banner -loglevel error -y "$@"
}

# A one-second GOP (30 fps, g=30): every video rendition below built with this places keyframes
# at identical sample positions, whatever their resolution or bitrate, which is what alignment
# requires. No B-frames, so no edit list, keeping these fixtures simple.
gop="-g 30 -keyint_min 30 -sc_threshold 0"

# The higher-bandwidth rendition of a two-rung ladder.
# shellcheck disable=SC2086
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -c:v libx264 -pix_fmt yuv420p -preset medium $gop -b:v 800k \
    -c:a aac -profile:a aac_low -b:a 128k \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/rendition-720p.mp4"

# The lower-bandwidth rung: same GOP, so its segments cut at the same instants as the 720p one.
# shellcheck disable=SC2086
ffmpeg_quiet \
    -f lavfi -i "$video_small" -f lavfi -i "$tone_a" \
    -c:v libx264 -pix_fmt yuv420p -preset medium $gop -b:v 250k \
    -c:a aac -profile:a aac_low -b:a 96k \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/rendition-480p.mp4"

# Same picture and GOP as rendition-720p.mp4, but a different keyframe interval: segments cut at
# different instants, so a composite built from both must be refused.
# shellcheck disable=SC2086
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -c:v libx264 -pix_fmt yuv420p -preset medium -g 24 -keyint_min 24 -sc_threshold 0 -b:v 800k \
    -c:a aac -profile:a aac_low -b:a 128k \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/rendition-misaligned.mp4"

# Dedicated audio-only renditions, two languages: when a mapper answer lists these, they form the
# shared audio group instead of either video rendition's own audio track.
ffmpeg_quiet \
    -f lavfi -i "$tone_a" \
    -c:a aac -profile:a aac_low -b:a 96k \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/rendition-audio-en.m4a"
ffmpeg_quiet \
    -f lavfi -i "$tone_b" \
    -c:a aac -profile:a aac_low -b:a 96k \
    -metadata:s:a:0 language=spa \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/rendition-audio-es.m4a"

printf 'Generated rendition MP4 fixtures in %s\n' "$fixture_dir"
