#!/usr/bin/env python3
"""Writes source-cipher-vectors.json: protected samples for segmentor's decryption unit tests.

Each vector is the same plaintext, (0..n) % 251, protected by the openssl-based reference
encryptor under one scheme and subsample layout. Run it from anywhere:
    python3 tests/fixtures/generate-source-vectors.py
"""
import json
import os
import sys

here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, here)
from cenc_reference import encrypt_sample  # noqa: E402

KEY = bytes.fromhex("00112233445566778899aabbccddeeff")

# name: (scheme, 16-byte IV, sample length, subsamples, crypt, skip)
CASES = {
    # An 8-byte IV, padded with zeros, over two protected ranges; the CTR stream runs across both.
    "cenc_two_ranges_iv8": ("cenc", bytes.fromhex("0102030405060708") + bytes(8), 100, [(10, 50), (5, 35)], 0, 0),
    # A whole sample with no subsample map, ending in a partial block that is still protected.
    "cenc_whole_sample_iv8": ("cenc", bytes.fromhex("a1a2a3a4a5a6a7a8") + bytes(8), 37, [], 0, 0),
    # 1:9 over one range of 15 whole blocks and 10 spare bytes: blocks 0 and 10 are protected.
    "cens_one_in_ten": ("cens", bytes.fromhex("0f0e0d0c0b0a09080706050403020100"), 262, [(12, 250)], 1, 9),
    # Whole blocks only; the chain runs on from the first range into the second.
    "cbc1_two_ranges": ("cbc1", bytes.fromhex("ffeeddccbbaa99887766554433221100"), 98, [(7, 48), (11, 32)], 0, 0),
    # The chain restarts from the IV in each range: block 0 and 10 of the first, block 0 of the second.
    "cbcs_one_in_ten_two_ranges": ("cbcs", bytes.fromhex("c0c1c2c3c4c5c6c7c8c9cacbcccdcecf"), 436, [(12, 250), (4, 170)], 1, 9),
    # Audio: no pattern, every whole block, the last 2 bytes clear.
    "cbcs_audio_every_block": ("cbcs", bytes.fromhex("c0c1c2c3c4c5c6c7c8c9cacbcccdcecf"), 50, [], 0, 0),
}

vectors = {}
for name, (scheme, iv, length, subsamples, crypt, skip) in CASES.items():
    plain = bytes(i % 251 for i in range(length))
    vectors[name] = {
        "scheme": scheme,
        "iv": iv.hex(),
        "subsamples": subsamples,
        "crypt": crypt,
        "skip": skip,
        "length": length,
        "cipher": encrypt_sample(scheme, KEY, iv, plain, subsamples, crypt, skip).hex(),
    }

with open(os.path.join(here, "source-cipher-vectors.json"), "w") as out:
    json.dump(vectors, out, indent=1, sort_keys=True)
    out.write("\n")
