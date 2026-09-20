#!/usr/bin/env sh
# Fixtures for the input shapes real encoders produce: edit lists, extra tracks, and codecs or
# layouts the packager does not support. Kept apart from generate.sh so regenerating these does
# not disturb the fixtures the FFprobe packet manifest was captured from.
set -eu

fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

video="testsrc2=size=320x180:rate=30:duration=3"
tone_a="sine=frequency=1000:sample_rate=48000:duration=3"
tone_b="sine=frequency=500:sample_rate=48000:duration=3"

ffmpeg_quiet() {
    ffmpeg -hide_banner -loglevel error -y "$@"
}

# ffmpeg's default output: an edit list on every track for B-frame delay and AAC priming.
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -c:v libx264 -pix_fmt yuv420p -preset medium -g 30 -keyint_min 30 -sc_threshold 0 -bf 2 \
    -c:a aac -profile:a aac_low -b:a 96k \
    -movflags +faststart \
    "$fixture_dir/h264-aac-default-edits.mp4"

# Audio delayed by half a second: the audio track gets a leading empty edit.
ffmpeg_quiet \
    -f lavfi -i "$video" -itsoffset 0.5 -f lavfi -i "$tone_a" \
    -c:v libx264 -pix_fmt yuv420p -preset medium -g 30 -keyint_min 30 -sc_threshold 0 -bf 2 \
    -c:a aac -profile:a aac_low -b:a 96k \
    -movflags +faststart \
    "$fixture_dir/h264-aac-audio-delay.mp4"

# Two audio tracks with different languages.
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" -f lavfi -i "$tone_b" \
    -map 0:v -map 1:a -map 2:a \
    -c:v libx264 -pix_fmt yuv420p -preset medium -g 30 -keyint_min 30 -sc_threshold 0 -bf 2 \
    -c:a aac -profile:a aac_low -b:a 96k \
    -metadata:s:a:0 language=eng -metadata:s:a:1 language=spa \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/h264-aac-two-audio.mp4"

# A timecode track next to the audio and video, as phones and cameras write.
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -c:v libx264 -pix_fmt yuv420p -preset medium -g 30 -keyint_min 30 -sc_threshold 0 -bf 2 \
    -c:a aac -profile:a aac_low -b:a 96k \
    -timecode 01:00:00:00 \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/h264-aac-timecode.mp4"

# HEVC: a codec the packager does not support yet.
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -c:v libx265 -tag:v hvc1 -pix_fmt yuv420p -preset ultrafast -x265-params log-level=error \
    -c:a aac -profile:a aac_low -b:a 96k \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/hevc-aac.mp4"

# Already-fragmented input, which the packager rejects with a specific message.
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -c:v libx264 -pix_fmt yuv420p -preset medium -g 30 -keyint_min 30 -sc_threshold 0 \
    -c:a aac -profile:a aac_low -b:a 96k \
    -movflags +frag_keyframe+empty_moov+default_base_moof \
    "$fixture_dir/h264-aac-fragmented.mp4"

printf 'Generated variant MP4 fixtures in %s\n' "$fixture_dir"
