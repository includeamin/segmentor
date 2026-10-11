#!/usr/bin/env python3
"""Protects a clear progressive MP4 under one of the CENC schemes, for segmentor's tests.

Usage: protect-fixture.py SCHEME INPUT OUTPUT [--whole-video]
SCHEME is cenc, cens, cbc1, or cbcs. With --whole-video the video is protected whole, with no
subsample map, so even its NAL unit length prefixes are encrypted; the default leaves them clear,
as real encoders do.

Every sample of every audio and video track is encrypted with cenc_reference.py, and the sample
entries gain `sinf`, `schm`, and `tenc`; the per-sample IVs and subsample maps go in a `senc`
box under `stbl` (or, in a fragmented file, one per `traf`). Video is protected NAL unit by NAL
unit, leaving the length prefix and the NAL header clear; audio is protected whole. The key and key ID are the ones in the tests.
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
CONTAINERS = {b"moov", b"trak", b"mdia", b"minf", b"stbl", b"moof", b"traf"}

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


def nal_subsamples(sample, length_size, header_size, whole_blocks):
    """One subsample per NAL unit: the length prefix and NAL header clear, the rest protected.
    The NAL header is 1 byte in H.264 and 2 in HEVC.

    With `whole_blocks` (cbc1, which cannot leave a partial block inside a protected range) the
    remainder after the last whole 16-byte block joins the clear bytes at the front instead.
    """
    subsamples = []
    at = 0
    while at < len(sample):
        size = int.from_bytes(sample[at:at + length_size], "big")
        assert size >= header_size and at + length_size + size <= len(sample), "malformed NAL unit"
        rest = size - header_size
        extra = rest % 16 if whole_blocks else 0
        subsamples.append((length_size + header_size + extra, rest - extra))
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


class Plan:
    """What protecting one track needs to know, found while its sample entry is rewritten."""

    def __init__(self, track_id, track_index, video, pattern, length_size, header_size):
        self.track_id = track_id
        self.track_index = track_index
        self.video = video
        self.pattern = pattern
        self.length_size = length_size
        self.header_size = header_size


def protect_entry(trak, track_index, scheme, config):
    """Rewrites the sample entry of `trak` as a protected one and returns its Plan."""
    stbl = trak.child(b"mdia").child(b"minf").child(b"stbl")
    handler = trak.child(b"mdia").child(b"hdlr").payload[8:12]
    video = handler == b"vide"
    pattern = config["video" if video else "audio"]
    stsd = stbl.child(b"stsd")
    entry_size, entry_name = struct.unpack(">I4s", stsd.payload[8:16])
    entry = stsd.payload[16:8 + entry_size]
    length_size = 4
    header_size = 1
    if video and b"hvcC" in entry:
        # HEVC: lengthSizeMinusOne is in byte 21 of the configuration, and NAL headers are 2 bytes.
        length_size = (entry[entry.index(b"hvcC") + 4 + 21] & 3) + 1
        header_size = 2
    elif video:
        length_size = (entry[entry.index(b"avcC") + 4 + 4] & 3) + 1
    protected_name = b"encv" if video else b"enca"
    protected = Box(protected_name, entry + sinf_box(entry_name, scheme, config, pattern))
    stsd.payload = stsd.payload[:8] + protected.build()
    track_id = struct.unpack(">I", trak.child(b"tkhd").payload[12:16])[0]
    return Plan(track_id, track_index, video, pattern, length_size, header_size)


def protect_sample(plan, scheme, config, whole_video, index, plain):
    """(protected bytes, IV, subsamples) of sample `index` of the track."""
    subsamples = []
    if plan.video and not whole_video:
        subsamples = nal_subsamples(plain, plan.length_size, plan.header_size, scheme == "cbc1")
    digest = hashlib.sha256(b"iv %d %d" % (plan.track_index, index)).digest()
    iv = digest[:16] if config["iv_size"] != 8 else digest[:8] + bytes(8)
    if config["iv_size"] == 0:
        iv = CONSTANT_IV
    crypt, skip = plan.pattern
    return encrypt_sample(scheme, KEY, iv, plain, subsamples, crypt, skip), iv, subsamples


def needs_senc(plan, config, whole_video):
    return bool(config["iv_size"] or (plan.video and not whole_video))


def protect_progressive(moov, boxes, data, scheme, config, whole_video):
    moov_at = 0
    for box in boxes:
        if box.kind == b"moov":
            break
        moov_at += len(box.build())
    old_size = len(moov.build())
    for index, trak in enumerate(c for c in moov.children if c.kind == b"trak"):
        plan = protect_entry(trak, index, scheme, config)
        stbl = trak.child(b"mdia").child(b"minf").child(b"stbl")
        sizes, offsets = sample_table(stbl)
        rows = []
        for number, (size, offset) in enumerate(zip(sizes, offsets)):
            cipher, iv, subsamples = protect_sample(
                plan, scheme, config, whole_video, number, bytes(data[offset:offset + size])
            )
            data[offset:offset + size] = cipher
            rows.append((iv, subsamples))
        if needs_senc(plan, config, whole_video):
            stbl.children.append(senc_box(rows, config, plan.video and not whole_video))
    # The media data moves by however much the movie box grew; shift every chunk offset to match.
    delta = len(moov.build()) - old_size
    for trak in (c for c in moov.children if c.kind == b"trak"):
        stco = trak.child(b"mdia").child(b"minf").child(b"stbl").child(b"stco")
        count = struct.unpack(">I", stco.payload[4:8])[0]
        offsets = struct.unpack(">%dI" % count, stco.payload[8:8 + 4 * count])
        stco.payload = stco.payload[:8] + struct.pack(">%dI" % count, *[o + delta for o in offsets])
    return bytes(data[:moov_at]) + moov.build() + bytes(data[moov_at + old_size:])


def read_trun(payload):
    """(flags, data_offset, sizes) of a `trun`; sizes must be listed per sample."""
    flags = struct.unpack(">I", payload[:4])[0] & 0xFFFFFF
    count = struct.unpack(">I", payload[4:8])[0]
    at = 8
    data_offset = None
    if flags & 0x1:
        data_offset = struct.unpack(">i", payload[at:at + 4])[0]
        at += 4
    if flags & 0x4:
        at += 4
    assert flags & 0x200, "this script needs a size in every trun sample"
    sizes = []
    for _ in range(count):
        if flags & 0x100:
            at += 4
        sizes.append(struct.unpack(">I", payload[at:at + 4])[0])
        at += 4
        if flags & 0x400:
            at += 4
        if flags & 0x800:
            at += 4
    return flags, data_offset, sizes


def protect_fragmented(moov, boxes, scheme, config, whole_video):
    plans = {}
    for index, trak in enumerate(c for c in moov.children if c.kind == b"trak"):
        plan = protect_entry(trak, index, scheme, config)
        plans[plan.track_id] = plan
    counters = {track_id: 0 for track_id in plans}
    out = []
    at = 0
    while at < len(boxes):
        box = boxes[at]
        if box.kind == b"mfra":
            at += 1  # its offsets would be stale, and nothing reads it
            continue
        if box.kind != b"moof":
            out.append(box)
            at += 1
            continue
        mdat = boxes[at + 1]
        assert mdat.kind == b"mdat", "a moof is followed by its mdat"
        old_size = len(box.build())
        media = bytearray(mdat.payload)
        for traf in (c for c in box.children if c.kind == b"traf"):
            tfhd = traf.child(b"tfhd").payload
            assert struct.unpack(">I", tfhd[:4])[0] & 0x20000, "this script needs default-base-is-moof"
            plan = plans[struct.unpack(">I", tfhd[4:8])[0]]
            rows = []
            for trun in (c for c in traf.children if c.kind == b"trun"):
                _, data_offset, sizes = read_trun(trun.payload)
                # The data is in the mdat that follows the moof, whose header is 8 bytes.
                position = data_offset - old_size - 8
                for size in sizes:
                    cipher, iv, subsamples = protect_sample(
                        plan, scheme, config, whole_video, counters[plan.track_id],
                        bytes(media[position:position + size]),
                    )
                    media[position:position + size] = cipher
                    rows.append((iv, subsamples))
                    counters[plan.track_id] += 1
                    position += size
            if needs_senc(plan, config, whole_video):
                traf.children.append(senc_box(rows, config, plan.video and not whole_video))
        # The moof grew, and the data it points at moved with the mdat that follows it.
        growth = len(box.build()) - old_size
        for traf in (c for c in box.children if c.kind == b"traf"):
            for trun in (c for c in traf.children if c.kind == b"trun"):
                payload = bytearray(trun.payload)
                offset = struct.unpack(">i", payload[8:12])[0] + growth
                payload[8:12] = struct.pack(">i", offset)
                trun.payload = bytes(payload)
        mdat.payload = bytes(media)
        out.extend([box, mdat])
        at += 2
    return out


def main(scheme, source, target, whole_video):
    config = SCHEMES[scheme]
    data = bytearray(open(source, "rb").read())
    boxes = parse(bytes(data))
    moov = next(b for b in boxes if b.kind == b"moov")
    if any(child.kind == b"mvex" for child in moov.children):
        out = protect_fragmented(moov, boxes, scheme, config, whole_video)
        result = b"".join(box.build() for box in out)
    else:
        result = protect_progressive(moov, boxes, data, scheme, config, whole_video)
    open(target, "wb").write(result)


if __name__ == "__main__":
    main(*sys.argv[1:4], whole_video="--whole-video" in sys.argv[4:])
