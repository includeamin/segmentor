#!/usr/bin/env sh
set -eu

fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
fixture="$fixture_dir/h264-aac.mp4"
probe="$fixture_dir/h264-aac.ffprobe.json"
moov_last="$fixture_dir/h264-aac-moov-last.mp4"
edit_list="$fixture_dir/h264-aac-edit-list.mp4"
video_only="$fixture_dir/h264-video-only.mp4"
aac_441_stereo="$fixture_dir/h264-aac-44100-stereo.mp4"
variable_timing="$fixture_dir/h264-variable-timing.mp4"

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
    -use_editlist 0 \
    -movflags +faststart \
    -y \
    "$fixture"

ffmpeg -hide_banner -loglevel error -i "$fixture" -map 0 -c copy -use_editlist 0 -y "$moov_last"
ffmpeg -hide_banner -loglevel error -i "$fixture" -map 0 -c copy -use_editlist 1 -movflags +faststart -y "$edit_list"
ffmpeg -hide_banner -loglevel error -i "$fixture" -map 0:v:0 -c copy -use_editlist 0 -movflags +faststart -y "$video_only"

ffmpeg \
    -hide_banner \
    -loglevel error \
    -f lavfi \
    -i "testsrc2=size=320x180:rate=30:duration=3" \
    -f lavfi \
    -i "sine=frequency=1000:sample_rate=44100:duration=3" \
    -filter_complex "[1:a]pan=stereo|c0=c0|c1=c0[a]" \
    -map 0:v:0 \
    -map "[a]" \
    -c:v libx264 \
    -pix_fmt yuv420p \
    -preset medium \
    -g 30 \
    -keyint_min 30 \
    -sc_threshold 0 \
    -bf 2 \
    -c:a aac \
    -profile:a aac_low \
    -b:a 128k \
    -use_editlist 0 \
    -movflags +faststart \
    -y \
    "$aac_441_stereo"

ffmpeg \
    -hide_banner \
    -loglevel error \
    -f lavfi \
    -i "testsrc2=size=320x180:rate=30:duration=3" \
    -vf "select='not(mod(n,2))+not(mod(n,5))'" \
    -fps_mode vfr \
    -c:v libx264 \
    -pix_fmt yuv420p \
    -preset medium \
    -g 30 \
    -keyint_min 30 \
    -sc_threshold 0 \
    -bf 2 \
    -use_editlist 0 \
    -movflags +faststart \
    -y \
    "$variable_timing"

# Run from the fixture directory on a relative path, so the manifest records `h264-aac.mp4` and
# not the absolute path of whoever regenerated it.
(cd "$fixture_dir" && ffprobe \
    -v error \
    -show_format \
    -show_streams \
    -show_packets \
    -of json \
    h264-aac.mp4) > "$probe"

printf 'Generated MP4 fixtures and %s\n' "$probe"