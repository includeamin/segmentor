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

# Video delayed by a second and a half: the video track gets a leading empty edit, so the whole
# presentation sits on a shifted timeline that sidecar subtitles must follow.
ffmpeg_quiet \
    -itsoffset 1.5 -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -c:v libx264 -pix_fmt yuv420p -preset medium -g 30 -keyint_min 30 -sc_threshold 0 -bf 2 \
    -c:a aac -profile:a aac_low -b:a 96k \
    -movflags +faststart \
    "$fixture_dir/h264-aac-video-delay.mp4"

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

# Fragmented input, remuxed with `-c copy` from the progressive fixture so that it holds exactly
# the same packets and a test can compare the two sample by sample. Each variant is a layout a
# real writer produces. This needs h264-aac.mp4, which generate.sh writes.
frag_source="$fixture_dir/h264-aac.mp4"

# Both tracks in each `moof`, base offsets relative to the `moof`.
ffmpeg_quiet -i "$frag_source" -c copy \
    -movflags +frag_keyframe+empty_moov+default_base_moof \
    "$fixture_dir/h264-aac-fragmented.mp4"

# No `default-base-is-moof`, so every `tfhd` carries an explicit base data offset.
ffmpeg_quiet -i "$frag_source" -c copy \
    -movflags +frag_keyframe+empty_moov \
    "$fixture_dir/h264-aac-fragmented-legacy.mp4"

# One track per `moof`, as CMAF writers produce.
ffmpeg_quiet -i "$frag_source" -c copy \
    -movflags +cmaf+separate_moof \
    "$fixture_dir/h264-aac-fragmented-cmaf.mp4"

# A `sidx` that a reader could use and must not need.
ffmpeg_quiet -i "$frag_source" -c copy \
    -movflags +frag_keyframe+empty_moov+default_base_moof+global_sidx \
    "$fixture_dir/h264-aac-fragmented-sidx.mp4"

# Version 1 `trun` boxes, whose composition offsets are signed.
ffmpeg_quiet -i "$frag_source" -c copy \
    -movflags +frag_keyframe+empty_moov+default_base_moof+negative_cts_offsets \
    "$fixture_dir/h264-aac-fragmented-negative-cts.mp4"

# A timeline that starts at 100 seconds, as recordings stamped with stream time do. FFmpeg
# normalises the start to zero, so this adds 100 seconds to every `tfdt` in the plain fragmented
# fixture, using each track's own timescale.
python3 - "$fixture_dir/h264-aac-fragmented.mp4" "$fixture_dir/h264-aac-fragmented-offset.mp4" <<'EOF'
import struct
import sys


def boxes(data, start, end):
    while start + 8 <= end:
        size, name = struct.unpack(">I4s", data[start:start + 8])
        yield name, start, start + size
        start += size


source, target = sys.argv[1:3]
data = bytearray(open(source, "rb").read())
timescales = {}
for name, start, end in boxes(data, 0, len(data)):
    if name != b"moov":
        continue
    for name, start, end in boxes(data, start + 8, end):
        if name != b"trak":
            continue
        track_id = timescale = None
        for name, inner, inner_end in boxes(data, start + 8, end):
            if name == b"tkhd":
                track_id = struct.unpack(">I", data[inner + 20:inner + 24])[0]
            if name == b"mdia":
                for name, deep, deep_end in boxes(data, inner + 8, inner_end):
                    if name == b"mdhd":
                        timescale = struct.unpack(">I", data[deep + 20:deep + 24])[0]
        timescales[track_id] = timescale
for name, start, end in boxes(data, 0, len(data)):
    if name != b"moof":
        continue
    for name, traf, traf_end in boxes(data, start + 8, end):
        if name != b"traf":
            continue
        track_id = base = None
        for name, inner, inner_end in boxes(data, traf + 8, traf_end):
            if name == b"tfhd":
                track_id = struct.unpack(">I", data[inner + 12:inner + 16])[0]
            if name == b"tfdt":
                wide = data[inner + 8] == 1
                base = inner + 12
        width, code = (8, ">Q") if wide else (4, ">I")
        (old,) = struct.unpack(code, data[base:base + width])
        data[base:base + width] = struct.pack(code, old + 100 * timescales[track_id])
open(target, "wb").write(data)
EOF

# Non-square pixels and tagged colour: the sample entry carries `pasp` and `colr`, which the
# init segment must keep or players render the wrong aspect and colours.
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -vf setsar=4:3 \
    -c:v libx264 -pix_fmt yuv420p -preset medium -g 30 -keyint_min 30 -sc_threshold 0 -bf 2 \
    -colorspace bt709 -color_primaries bt709 -color_trc bt709 \
    -c:a aac -profile:a aac_low -b:a 96k \
    -use_editlist 0 -movflags +faststart+write_colr \
    "$fixture_dir/h264-aac-anamorphic.mp4"

# QuickTime: versioned `mp4a` sample entries with a `wave` box, and a `qt  ` brand.
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -c:v libx264 -pix_fmt yuv420p -preset medium -g 30 -keyint_min 30 -sc_threshold 0 -bf 2 \
    -c:a aac -profile:a aac_low -b:a 96k \
    -use_editlist 0 -movflags +faststart -f mov \
    "$fixture_dir/h264-aac-quicktime.mov"

# Codecs beyond H.264 and AAC-LC. Each carries a one-second keyframe interval so segments cut.
h264="-c:v libx264 -pix_fmt yuv420p -preset medium -g 30 -keyint_min 30 -sc_threshold 0"

ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -c:v libvpx-vp9 -b:v 0 -crf 40 -g 30 -keyint_min 30 -deadline realtime -cpu-used 8 \
    -c:a libopus -b:a 64k -strict -2 \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/vp9-opus.mp4"

ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    -c:v libsvtav1 -preset 10 -g 30 \
    -c:a aac -profile:a aac_low -b:a 96k \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/av1-aac.mp4"

# shellcheck disable=SC2086
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    $h264 -c:a ac3 -b:a 192k \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/h264-ac3.mp4"

# shellcheck disable=SC2086
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    $h264 -c:a eac3 -b:a 192k \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/h264-eac3.mp4"

# shellcheck disable=SC2086
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    $h264 -c:a flac -strict -2 \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/h264-flac.mp4"

# Audio only, with FFmpeg's default edit list for AAC priming.
ffmpeg_quiet \
    -f lavfi -i "$tone_a" \
    -c:a aac -profile:a aac_low -b:a 96k \
    -movflags +faststart \
    "$fixture_dir/aac-only.m4a"

# Two audio tracks and no video.
ffmpeg_quiet \
    -f lavfi -i "$tone_a" -f lavfi -i "$tone_b" \
    -map 0:a -map 1:a \
    -c:a aac -profile:a aac_low -b:a 96k \
    -metadata:s:a:0 language=eng -metadata:s:a:1 language=spa \
    -use_editlist 0 -movflags +faststart \
    "$fixture_dir/aac-two-tracks-only.m4a"

# MP3 inside MP4: a codec the packager does not support, in a sample entry (`mp4a`) that looks
# like one it does. The rejection must name the audio object type.
# shellcheck disable=SC2086
ffmpeg_quiet \
    -f lavfi -i "$video" -f lavfi -i "$tone_a" \
    $h264 -c:a libmp3lame -b:a 128k \
    -use_editlist 0 -movflags +faststart -f mp4 \
    "$fixture_dir/h264-mp3.mp4"


# Encrypted sources (TDD 0014). First FFmpeg's own cenc-aes-ctr output, with the senc, saio, and
# saiz boxes a real encoder writes under stbl. FFmpeg picks random IVs, so regenerating this
# file changes its bytes; the tests check what it must look like, not those bytes.
ffmpeg_quiet \
    -i "$fixture_dir/h264-aac.mp4" -c copy -movflags +faststart \
    -encryption_scheme cenc-aes-ctr \
    -encryption_key 00112233445566778899aabbccddeeff \
    -encryption_kid 0123456789abcdef0123456789abcdef \
    "$fixture_dir/h264-aac-cenc-ffmpeg.mp4"

# The same clear file protected under each of the four schemes by the openssl-based reference
# encryptor, which shares no code with segmentor. FFmpeg decrypts cenc, cbc1, and cbcs to the
# clear file's exact frames; it has no cens pattern support, so cens has no second opinion.
for scheme in cenc cens cbc1 cbcs; do
    python3 "$fixture_dir/protect-fixture.py" "$scheme" \
        "$fixture_dir/h264-aac.mp4" "$fixture_dir/h264-aac-$scheme.mp4"
done

# A file with edit lists (FFmpeg's default output), protected: the audio track's leading priming
# samples are dropped by the edit list, which must not shift which IV and subsample map each
# remaining sample gets.
python3 "$fixture_dir/protect-fixture.py" cenc \
    "$fixture_dir/h264-aac-default-edits.mp4" "$fixture_dir/h264-aac-default-edits-cenc.mp4"

# Fragmented sources (TDD 0014, stage 2): the CMAF fixture protected under each scheme, with a
# senc in every traf. Then FFmpeg's own fragmented cenc output. Its video is correct. Its audio
# is not: from the second fragment on, each audio senc lists every sample so far, not the
# fragment's own, which breaks ISO/IEC 23001-7 (and is why FFmpeg cannot decode that audio
# itself). Segmentor must refuse that file; the video-only copy is the one it must decrypt.
for scheme in cenc cens cbc1 cbcs; do
    python3 "$fixture_dir/protect-fixture.py" "$scheme" \
        "$fixture_dir/h264-aac-fragmented-cmaf.mp4" \
        "$fixture_dir/h264-aac-fragmented-cmaf-$scheme.mp4"
done
ffmpeg_quiet \
    -i "$fixture_dir/h264-aac.mp4" -c copy \
    -movflags frag_keyframe+empty_moov+default_base_moof \
    -encryption_scheme cenc-aes-ctr \
    -encryption_key 00112233445566778899aabbccddeeff \
    -encryption_kid 0123456789abcdef0123456789abcdef \
    "$fixture_dir/h264-aac-fragmented-cenc-ffmpeg.mp4"
ffmpeg_quiet \
    -i "$fixture_dir/h264-aac.mp4" -an -c copy \
    -movflags frag_keyframe+empty_moov+default_base_moof \
    -encryption_scheme cenc-aes-ctr \
    -encryption_key 00112233445566778899aabbccddeeff \
    -encryption_kid 0123456789abcdef0123456789abcdef \
    "$fixture_dir/h264-video-fragmented-cenc-ffmpeg.mp4"

# HEVC, protected: its codec string comes from the `hvcC` of the entry the protection hides.
python3 "$fixture_dir/protect-fixture.py" cenc \
    "$fixture_dir/hevc-aac.mp4" "$fixture_dir/hevc-aac-cenc.mp4"

# Video protected whole, with no subsample map, so even the NAL unit length prefixes are
# encrypted: the one layout in which a wrong key can be noticed when the asset loads.
python3 "$fixture_dir/protect-fixture.py" cenc \
    "$fixture_dir/h264-aac.mp4" "$fixture_dir/h264-aac-cenc-whole-video.mp4" --whole-video

printf 'Generated variant MP4 fixtures in %s\n' "$fixture_dir"
