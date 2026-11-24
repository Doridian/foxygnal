#!/usr/bin/env python3
"""Known-answer strips for the Meteor-M LRPT (MSU-MR) image codec, from libjpeg.

    scripts/lrpt-jpeg-vectors.py [OUT_FILE]

An MSU-MR image packet carries 14 blocks of 8 x 8 pixels coded exactly like
the entropy-coded segment of a baseline greyscale JPEG that uses the standard
(Annex K) luminance quantization and Huffman tables, minus the markers and
the 0xFF byte stuffing, with the DC prediction starting at 0. So libjpeg,
through Pillow, makes independent test vectors: a 112 x 8 picture saved as a
baseline JPEG at quality Q (Pillow scales the standard table with the IJG
formula, which this script checks against the file's DQT), its scan data cut
out and unstuffed, and libjpeg's own decode of the file as the answer.

Writes (default crates/foxygnal-blocks/tests/fixtures/lrpt/jpeg-strips.bin)
one record per strip: quality (u8), coded length (u16 LE), the coded bytes,
then 896 decoded pixels (row-major, 8 rows of 112).

The pictures are deterministic (a seeded numpy generator): smooth gradients,
edges, noise, flat grey, black and white, at qualities 20 to 95.
"""
import io
import os
import sys

import numpy as np
from PIL import Image

STD = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57,
    69, 56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64,
    81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
]
ZIGZAG = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27,
    20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
]


def ijg(q):
    scale = 5000 // q if q < 50 else 200 - 2 * q
    return [max(1, min(255, (s * scale + 50) // 100)) for s in STD]


def segments(jpg):
    """(marker, payload) up to SOS, then ('scan', entropy data unstuffed)."""
    i = 2
    out = []
    while i < len(jpg):
        assert jpg[i] == 0xFF
        m = jpg[i + 1]
        n = int.from_bytes(jpg[i + 2:i + 4], "big")
        out.append((m, jpg[i + 4:i + 2 + n]))
        i += 2 + n
        if m == 0xDA:
            break
    scan = bytearray()
    while True:
        b = jpg[i]
        if b == 0xFF:
            nxt = jpg[i + 1]
            if nxt == 0x00:
                scan.append(0xFF)
                i += 2
                continue
            assert nxt == 0xD9, hex(nxt)
            break
        scan.append(b)
        i += 1
    out.append(("scan", bytes(scan)))
    return out


def strip(pixels, q):
    img = Image.frombytes("L", (112, 8), pixels.astype(np.uint8).tobytes())
    buf = io.BytesIO()
    img.save(buf, "JPEG", quality=q, optimize=False, progressive=False)
    jpg = buf.getvalue()
    segs = segments(jpg)
    dqt = [p for m, p in segs if m == 0xDB]
    assert len(dqt) == 1 and dqt[0][0] == 0, "one 8-bit table expected"
    table = list(dqt[0][1:65])
    natural = [0] * 64
    for k, v in enumerate(table):
        natural[ZIGZAG[k]] = v
    assert natural == ijg(q), f"Q {q}: DQT differs from the IJG scaling"
    dht = [p for m, p in segs if m == 0xC4]
    # The standard luminance DC (class 0) and AC (class 1) tables.
    tables = b"".join(dht)
    assert bytes([0x00, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0]) in tables
    assert bytes([0x10, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D]) in tables
    coded = [p for m, p in segs if m == "scan"][0]
    decoded = Image.open(io.BytesIO(jpg)).tobytes()
    assert len(decoded) == 896
    return coded, decoded


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        os.path.dirname(__file__), "..", "crates", "foxygnal-blocks", "tests", "fixtures",
        "lrpt", "jpeg-strips.bin")
    rng = np.random.default_rng(20261123)
    y, x = np.mgrid[0:8, 0:112]
    pictures = [
        128 + 100 * np.sin(x * 0.05) * np.cos(y * 0.3),
        np.where((x // 9 + y // 3) % 2 == 0, 30, 220),
        np.clip(rng.normal(128, 40, (8, 112)), 0, 255),
        np.full((8, 112), 128),
        np.full((8, 112), 0),
        np.full((8, 112), 255),
        np.clip(x * 2.3 + y * 5, 0, 255),
        np.clip(rng.normal(128, 90, (8, 112)), 0, 255),
        np.clip(60 + 0.8 * x + 40 * (x > 56), 0, 255),
    ]
    with open(out, "wb") as f:
        n = 0
        for qs, pic in zip([[20, 75], [50, 90], [30, 95], [60], [25], [85], [45, 70], [80], [55]],
                           pictures):
            for q in qs:
                coded, decoded = strip(np.asarray(pic), q)
                f.write(bytes([q]) + len(coded).to_bytes(2, "little") + coded + decoded)
                n += 1
    print(f"{n} strips -> {out}")


if __name__ == "__main__":
    main()
