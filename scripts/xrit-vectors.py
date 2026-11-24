#!/usr/bin/env python3
"""Known-answer vectors for xRIT Rice decompression, made with libaec.

    scripts/xrit-vectors.py [OUT_DIR]
    scripts/xrit-vectors.py --check FILE

Writes `rice.bin` (default: crates/foxygnal-blocks/tests/fixtures/xrit) for
`foxygnal-blocks` to check its CCSDS 121 / szip decoder against, so CI needs
no libaec. Made with libaec 1.1.7's szip interface (`SZ_BufftoBuffCompress`
in libsz), the library GOES receivers (goestools, SatDump) decompress with.
szip refuses scan lines over 4096 pixels, so the one longer case (a GOES
full-disk line, 5424 pixels) goes through libaec's own `aec_buffer_encode`
with the flags szip would set (MSB, preprocessing, no RSI padding).

It also writes small JPEG files and libjpeg's decoding of them (see
`write_jpeg`), for the JPEG pictures of GK-2A and Elektro-L.

`rice.bin` is a sequence of cases, each (big-endian):

    u16 options   szip options mask (49 = K13 | MSB | NN, as GOES sends)
    u8  bpp       bits per pixel
    u8  ppb       pixels per block
    u16 ppl       pixels per scan line
    u16 lines     scan lines
    u32 n         bytes of compressed data
    raw           lines * ppl pixels, 1 byte each up to 8 bits, else 2
                  (in the byte order the options say)
    compressed    n bytes from SZ_BufftoBuffCompress

`--check FILE` reads the same format, written by foxygnal's encoder (the
`rice_encoder_output` test, run with FOXYGNAL_RICE_OUT=FILE), decompresses
each case with libaec and compares: the other direction of the check.

The pixels are deterministic (the LCG x <- 1103515245*x + 12345 mod 2^32,
seed 1, bits 16..30), shaped per case to give smooth gradients, flat areas
(zero-block runs), small residuals (the second extension) and noise
(uncompressed blocks).
"""
import ctypes
import os
import struct
import sys

sz = ctypes.CDLL("libsz.so.2")
aec = ctypes.CDLL("libaec.so.0")


class AecStream(ctypes.Structure):
    _fields_ = [
        ("next_in", ctypes.c_void_p),
        ("avail_in", ctypes.c_size_t),
        ("total_in", ctypes.c_size_t),
        ("next_out", ctypes.c_void_p),
        ("avail_out", ctypes.c_size_t),
        ("total_out", ctypes.c_size_t),
        ("bits_per_sample", ctypes.c_uint),
        ("block_size", ctypes.c_uint),
        ("rsi", ctypes.c_uint),
        ("flags", ctypes.c_uint),
        ("state", ctypes.c_void_p),
    ]


def aec_run(fn, data, size, opts, bpp, ppb, ppl):
    """libaec's own interface, with the flags szip would set: for scan lines
    longer than szip's 4096-pixel limit (one GOES full-disk line)."""
    assert ppl % ppb == 0
    inp = ctypes.create_string_buffer(bytes(data), len(data))
    out = ctypes.create_string_buffer(size)
    flags = (4 if opts & 16 else 0) | (8 if opts & 32 else 0)
    s = AecStream(
        ctypes.cast(inp, ctypes.c_void_p), len(data), 0,
        ctypes.cast(out, ctypes.c_void_p), size, 0,
        bpp, ppb, ppl // ppb, flags, None,
    )
    r = fn(ctypes.byref(s))
    if r != 0:
        raise RuntimeError(f"libaec: {r}")
    return out.raw[: s.total_out]


class SZ(ctypes.Structure):
    _fields_ = [
        ("options_mask", ctypes.c_int),
        ("bits_per_pixel", ctypes.c_int),
        ("pixels_per_block", ctypes.c_int),
        ("pixels_per_scanline", ctypes.c_int),
    ]


def compress(data, opts, bpp, ppb, ppl):
    if ppl > 4096:
        return aec_run(aec.aec_buffer_encode, data, len(data) * 2 + 4096, opts, bpp, ppb, ppl)
    p = SZ(opts, bpp, ppb, ppl)
    out = ctypes.create_string_buffer(len(data) * 2 + 4096)
    n = ctypes.c_size_t(len(out))
    r = sz.SZ_BufftoBuffCompress(out, ctypes.byref(n), data, len(data), ctypes.byref(p))
    if r != 0:
        raise RuntimeError(f"SZ_BufftoBuffCompress: {r}")
    return out.raw[: n.value]


def decompress(data, size, opts, bpp, ppb, ppl):
    if ppl > 4096:
        return aec_run(aec.aec_buffer_decode, data, size, opts, bpp, ppb, ppl)
    p = SZ(opts, bpp, ppb, ppl)
    out = ctypes.create_string_buffer(size)
    n = ctypes.c_size_t(size)
    r = sz.SZ_BufftoBuffDecompress(out, ctypes.byref(n), data, len(data), ctypes.byref(p))
    if r != 0:
        raise RuntimeError(f"SZ_BufftoBuffDecompress: {r}")
    return out.raw[: n.value]


class Lcg:
    def __init__(self):
        self.x = 1

    def next(self):
        self.x = (self.x * 1103515245 + 12345) & 0xFFFFFFFF
        return (self.x >> 16) & 0x7FFF


def pixels(lcg, shape, bpp, ppl, lines):
    top = (1 << bpp) - 1
    out = []
    for y in range(lines):
        for x in range(ppl):
            r = lcg.next()
            if shape == "earth":
                # A disc with smooth shading and a little noise, black space
                # around it (long zero-block runs).
                cx, cy, rad = ppl / 2, lines * 4, ppl * 0.45
                d2 = (x - cx) ** 2 + (y * 8 - cy) ** 2
                v = 0
                if d2 < rad * rad:
                    v = int(top * (0.3 + 0.5 * (1 - d2 / (rad * rad)))) + (r & 3)
            elif shape == "flat":
                v = top // 3
            elif shape == "small":
                v = top // 2 + (r % 3) - 1
            elif shape == "noise":
                v = r
            else:  # ramp
                v = (x * 3 + y * 7) // 2 + (r & 1)
            out.append(max(0, min(top, v)))
    return out


def pack(values, bpp, opts):
    if bpp <= 8:
        return bytes(values)
    fmt = ">H" if opts & 16 else "<H"
    return b"".join(struct.pack(fmt, v) for v in values)


CASES = [
    # options, bpp, ppb, ppl, lines, shape
    (49, 8, 16, 5424, 1, "earth"),  # one GOES full-disk line, as one packet
    (49, 8, 16, 1000, 4, "earth"),  # partial last block (mesoscale width)
    (49, 8, 16, 2048, 2, "flat"),  # zero-block runs to the end of segments
    (49, 8, 16, 256, 4, "small"),  # second extension
    (49, 8, 8, 64, 8, "noise"),  # uncompressed blocks
    (17, 8, 16, 128, 4, "ramp"),  # no prediction
    (49, 10, 16, 300, 3, "earth"),
    (41, 12, 32, 256, 2, "ramp"),  # LSB first
    (49, 16, 32, 256, 2, "noise"),
    (49, 16, 16, 100, 3, "small"),
]


def write(out_dir):
    lcg = Lcg()
    blob = bytearray()
    for opts, bpp, ppb, ppl, lines, shape in CASES:
        raw = pack(pixels(lcg, shape, bpp, ppl, lines), bpp, opts)
        comp = compress(raw, opts, bpp, ppb, ppl)
        assert decompress(comp, len(raw), opts, bpp, ppb, ppl) == raw
        blob += struct.pack(">HBBHHI", opts, bpp, ppb, ppl, lines, len(comp))
        blob += raw + comp
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, "rice.bin"), "wb") as f:
        f.write(blob)
    print(f"rice.bin: {len(CASES)} cases, {len(blob)} bytes")
    write_jpeg(out_dir)


def write_jpeg(out_dir):
    """`jpeg-grey.jpg` / `jpeg-rgb.jpg` (baseline, made by Pillow's libjpeg)
    and `jpeg-grey.raw` / `jpeg-rgb.raw`, the pixels libjpeg decodes them
    to: GK-2A and Elektro-L send JPEG pictures."""
    from PIL import Image

    lcg = Lcg()
    w, h = 48, 32
    grey = Image.new("L", (w, h))
    grey.putdata([(x * 5 + y * 3 + (lcg.next() & 7)) & 255 for y in range(h) for x in range(w)])
    rgb = Image.new("RGB", (w, h))
    rgb.putdata([(x * 5, y * 7, (x * y) & 255) for y in range(h) for x in range(w)])
    for name, im in (("grey", grey), ("rgb", rgb)):
        path = os.path.join(out_dir, f"jpeg-{name}.jpg")
        im.save(path, "JPEG", quality=90)
        with open(os.path.join(out_dir, f"jpeg-{name}.raw"), "wb") as f:
            f.write(Image.open(path).tobytes())


def check(path):
    data = open(path, "rb").read()
    at, n = 0, 0
    while at < len(data):
        opts, bpp, ppb, ppl, lines, clen = struct.unpack_from(">HBBHHI", data, at)
        at += 12
        size = ppl * lines * (1 if bpp <= 8 else 2)
        raw, comp = data[at : at + size], data[at + size : at + size + clen]
        at += size + clen
        got = decompress(comp, size, opts, bpp, ppb, ppl)
        if got != raw:
            sys.exit(f"case {n} ({opts} {bpp} {ppb} {ppl}x{lines}): libaec disagrees")
        n += 1
    print(f"libaec decodes all {n} cases")


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--check":
        check(sys.argv[2])
    else:
        here = os.path.dirname(os.path.abspath(__file__))
        default = os.path.join(here, "..", "crates", "foxygnal-blocks", "tests", "fixtures", "xrit")
        write(sys.argv[1] if len(sys.argv) > 1 else default)
