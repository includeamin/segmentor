"""A reference encryptor for the CENC protection schemes (ISO/IEC 23001-7), for test fixtures.

It is built on the `openssl` command line and shares no code with vodnik, so a fixture
protected by it is an independent check of vodnik's decryption.
"""
import subprocess


def _openssl(mode, key, iv, data):
    """AES-128 in `mode` ("ctr" or "cbc") over `data`, with no padding."""
    if not data:
        return b""
    return subprocess.run(
        ["openssl", "enc", "-aes-128-" + mode, "-nopad", "-K", key.hex(), "-iv", iv.hex()],
        input=data,
        capture_output=True,
        check=True,
    ).stdout


def protected_ranges(total, subsamples):
    """The (start, length) of every protected range of a sample of `total` bytes.

    `subsamples` is a list of (clear, protected) byte counts; empty means the whole sample.
    """
    if not subsamples:
        return [(0, total)]
    ranges = []
    at = 0
    for clear, protected in subsamples:
        at += clear
        ranges.append((at, protected))
        at += protected
    assert at == total, "subsamples must cover the sample exactly"
    return ranges


def crypt_blocks(length, crypt, skip):
    """The indices of the whole 16-byte blocks of a range that a pattern encrypts."""
    whole = length // 16
    if crypt == 0 and skip == 0:
        return list(range(whole))
    return [index for index in range(whole) if index % (crypt + skip) < crypt]


def encrypt_sample(scheme, key, iv, plain, subsamples, crypt=0, skip=0):
    """`plain` protected under `scheme` (cenc, cens, cbc1, or cbcs) with a 16-byte `iv`."""
    out = bytearray(plain)
    ranges = protected_ranges(len(plain), subsamples)
    if scheme == "cenc":
        # One CTR stream over the protected bytes of the sample, partial blocks included.
        stream = b"".join(plain[start:start + length] for start, length in ranges)
        encrypted = _openssl("ctr", key, iv, stream)
        at = 0
        for start, length in ranges:
            out[start:start + length] = encrypted[at:at + length]
            at += length
    elif scheme in ("cens", "cbc1"):
        # cens: the pattern's blocks of every range form one CTR stream.
        # cbc1: every whole block of every range forms one CBC chain.
        places = []
        for start, length in ranges:
            if scheme == "cens":
                blocks = crypt_blocks(length, crypt, skip)
            else:
                blocks = range(length // 16)
            places.extend(start + 16 * block for block in blocks)
        stream = b"".join(plain[place:place + 16] for place in places)
        encrypted = _openssl("ctr" if scheme == "cens" else "cbc", key, iv, stream)
        for index, place in enumerate(places):
            out[place:place + 16] = encrypted[16 * index:16 * index + 16]
    elif scheme == "cbcs":
        # The chain starts again from the IV in every protected range, and skips clear blocks.
        for start, length in ranges:
            places = [start + 16 * block for block in crypt_blocks(length, crypt, skip)]
            stream = b"".join(plain[place:place + 16] for place in places)
            encrypted = _openssl("cbc", key, iv, stream)
            for index, place in enumerate(places):
                out[place:place + 16] = encrypted[16 * index:16 * index + 16]
    else:
        raise ValueError("unknown scheme " + scheme)
    return bytes(out)
