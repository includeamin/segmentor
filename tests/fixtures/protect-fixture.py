#!/usr/bin/env python3
"""Protects a clear progressive MP4 under one of the CENC schemes, for segmentor's tests.

Usage: protect-fixture.py SCHEME INPUT OUTPUT [--whole-video]
SCHEME is cenc, cens, cbc1, or cbcs. With --whole-video the video is protected whole, with no
subsample map, so even its NAL unit length prefixes are encrypted; the default leaves them clear,
as real encoders do.

Every sample of every audio and video track is encrypted with cenc_reference.py, and the sample
entries gain `sinf`, `schm`, and `tenc`; the per-sample IVs and subsample maps go in a `senc`
box under `stbl`. Video is protected NAL unit by NAL unit, leaving the length prefix and the
NAL header clear; audio is protected whole. The key and key ID are the ones in the tests.
"""
import hashlib
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cenc_reference import encrypt_sample  # noqa: E402

KEY = bytes.fromhex("00112233445566778899aabbccddeeff")
KEY_ID = bytes.fromhex("0123456789abcdef0123456789abcdef")
CONSTANT_IV = bytes.fromhex("c0c1c2c3c4c5c6c7c8c9cacbcccdcecf")
CONTAINERS = {b"moov", b"trak", b"mdia", b"minf", b"stbl"}

# Per scheme: the tenc version, the per-sample IV size, and the (crypt, skip) pattern of video
# and of audio.
SCHEMES = {
    "cenc": {"tenc": 0, "iv_size": 8, "video": (0, 0), "audio": (0, 0)},
    "cens": {"tenc": 1, "iv_size": 8, "video": (1, 9), "audio": (1, 9)},
    "cbc1": {"tenc": 0, "iv_size": 16, "video": (0, 0), "audio": (0, 0)},
    "cbcs": {"tenc": 1, "iv_size": 0, "video": (1, 9), "audio": (0, 0)},
}


class Box:
    def __init__(self, kind, payload=b"", children=None):
        self.kind = kind
        self.payload = payload
        self.children = children

    def build(self):
        body = self.payload if self.children is None else b"".join(c.build() for c in self.children)
        return struct.pack(">I4s", 8 + len(body), self.kind) + body

    def child(self, kind):
        return next(c for c in self.children if c.kind == kind)


def parse(data):
    boxes = []
    at = 0
    while at < len(data):
        size, kind = struct.unpack(">I4s", data[at:at + 8])
        assert size >= 8 and at + size <= len(data), "unsupported box layout"
        body = data[at + 8:at + size]
        if kind in CONTAINERS:
            boxes.append(Box(kind, children=parse(body)))
        else:
            boxes.append(Box(kind, body))
        at += size
    return boxes


def sample_table(stbl):
    """(sizes, absolute offsets) of every sample, from stsz, stsc, and stco."""
    stsz = stbl.child(b"stsz").payload
    fixed, count = struct.unpack(">II", stsz[4:12])
    sizes = [fixed] * count if fixed else list(struct.unpack(">%dI" % count, stsz[12:12 + 4 * count]))
    stsc = stbl.child(b"stsc").payload
    entries = struct.unpack(">I", stsc[4:8])[0]
    runs = [struct.unpack(">III", stsc[8 + 12 * i:20 + 12 * i]) for i in range(entries)]
    stco = stbl.child(b"stco").payload
    chunks = struct.unpack(">I", stco[4:8])[0]
    offsets = struct.unpack(">%dI" % chunks, stco[8:8 + 4 * chunks])
    result = []
    sample = 0
    for chunk in range(1, chunks + 1):
        per_chunk = [r[1] for r in runs if r[0] <= chunk][-1]
        at = offsets[chunk - 1]
        for _ in range(per_chunk):
            result.append(at)
            at += sizes[sample]
            sample += 1
    assert sample == count
    return sizes, result


def nal_subsamples(sample, length_size, whole_blocks):
    """One subsample per NAL unit: the length prefix and NAL header clear, the rest protected.

    With `whole_blocks` (cbc1, which cannot leave a partial block inside a protected range) the
    remainder after the last whole 16-byte block joins the clear bytes at the front instead.
    """
    subsamples = []
    at = 0
    while at < len(sample):
        size = int.from_bytes(sample[at:at + length_size], "big")
        assert size >= 1 and at + length_size + size <= len(sample), "malformed NAL unit"
        rest = size - 1
        extra = rest % 16 if whole_blocks else 0
        subsamples.append((length_size + 1 + extra, rest - extra))
        at += length_size + size
    return subsamples


def sinf_box(original, scheme, config, pattern):
    tenc = bytes([config["tenc"], 0, 0, 0, 0])
    tenc += bytes([(pattern[0] << 4) | pattern[1]]) if config["tenc"] else b"\x00"
    tenc += bytes([1, config["iv_size"]]) + KEY_ID
    if config["iv_size"] == 0:
        tenc += bytes([16]) + CONSTANT_IV
    return Box(
        b"sinf",
        children=[
            Box(b"frma", original),
            Box(b"schm", bytes(4) + scheme.encode() + struct.pack(">I", 0x00010000)),
            Box(b"schi", children=[Box(b"tenc", tenc)]),
        ],
    ).build()


def senc_box(rows, config, with_subsamples):
    flags = 2 if with_subsamples else 0
    body = struct.pack(">II", flags, len(rows))
    for iv, subsamples in rows:
        body += iv[: config["iv_size"]]
        if with_subsamples:
            body += struct.pack(">H", len(subsamples))
            for clear, protected in subsamples:
                body += struct.pack(">HI", clear, protected)
    return Box(b"senc", body)


def protect_track(trak, track_index, scheme, config, data, whole_video):
    stbl = trak.child(b"mdia").child(b"minf").child(b"stbl")
    handler = trak.child(b"mdia").child(b"hdlr").payload[8:12]
    video = handler == b"vide"
    pattern = config["video" if video else "audio"]
    stsd = stbl.child(b"stsd")
    entry_size, entry_name = struct.unpack(">I4s", stsd.payload[8:16])
    entry = stsd.payload[16:8 + entry_size]
    length_size = 4
    if video:
        avcc = entry.index(b"avcC")
        length_size = (entry[avcc + 4 + 4] & 3) + 1
    sizes, offsets = sample_table(stbl)

    rows = []
    for index, (size, offset) in enumerate(zip(sizes, offsets)):
        plain = bytes(data[offset:offset + size])
        subsamples = []
        if video and not whole_video:
            subsamples = nal_subsamples(plain, length_size, scheme == "cbc1")
        digest = hashlib.sha256(b"iv %d %d" % (track_index, index)).digest()
        iv = digest[:16] if config["iv_size"] != 8 else digest[:8] + bytes(8)
        if config["iv_size"] == 0:
            iv = CONSTANT_IV
        data[offset:offset + size] = encrypt_sample(
            scheme, KEY, iv, plain, subsamples, pattern[0], pattern[1]
        )
        rows.append((iv, subsamples))

    protected_name = b"encv" if video else b"enca"
    protected = Box(protected_name, entry + sinf_box(entry_name, scheme, config, pattern))
    stsd.payload = stsd.payload[:8] + protected.build()
    with_subsamples = video and not whole_video
    if config["iv_size"] or with_subsamples:
        stbl.children.append(senc_box(rows, config, with_subsamples))


def main(scheme, source, target, whole_video):
    config = SCHEMES[scheme]
    data = bytearray(open(source, "rb").read())
    boxes = parse(bytes(data))
    moov_at = 0
    for box in boxes:
        if box.kind == b"moov":
            break
        moov_at += len(box.build())
    moov = next(b for b in boxes if b.kind == b"moov")
    old_size = len(moov.build())
    for index, trak in enumerate(c for c in moov.children if c.kind == b"trak"):
        protect_track(trak, index, scheme, config, data, whole_video)
    # The media data moves by however much the movie box grew; shift every chunk offset to match.
    delta = len(moov.build()) - old_size
    for trak in (c for c in moov.children if c.kind == b"trak"):
        stco = trak.child(b"mdia").child(b"minf").child(b"stbl").child(b"stco")
        count = struct.unpack(">I", stco.payload[4:8])[0]
        offsets = struct.unpack(">%dI" % count, stco.payload[8:8 + 4 * count])
        stco.payload = stco.payload[:8] + struct.pack(">%dI" % count, *[o + delta for o in offsets])
    out = bytes(data[:moov_at]) + moov.build() + bytes(data[moov_at + old_size:])
    open(target, "wb").write(out)


if __name__ == "__main__":
    main(*sys.argv[1:4], whole_video="--whole-video" in sys.argv[4:])
