#!/usr/bin/env python3
"""Known-answer vectors for the CCSDS layer, from independent implementations.

    scripts/ccsds-vectors.py [OUT_DIR]

Writes small binary files (default: crates/foxygnal-blocks/tests/fixtures/ccsds)
that `foxygnal-blocks` checks its CCSDS code against, so CI needs neither
library. Made with GNU Radio 3.10.12, whose libgnuradio-fec carries Phil Karn's
libfec Reed-Solomon (`encode_rs_ccsds`: RS(255,223), dual basis) and the
`encode_ccsds_27_bb` convolutional encoder.

The data are bytes of the LCG x <- 1103515245*x + 12345 (mod 2^32, seed 1),
each byte bits 16..23 of the new x, continuing from file to file in the order
below. Files:

  rs-i1.bin    RS(255,223) codewords from encode_rs_ccsds, dual basis, data
               then parity: 3 whole (255 bytes each), then shortened by 33
               (222 bytes) and by 95 (160 bytes)
  rs-i4.bin    one 1020-byte codeblock, I = 4: data byte j of codeword
               j mod 4, parity likewise (the usual 1024-byte CADU without
               its ASM, not randomized)
  rs-i5.bin    one 1275-byte codeblock, I = 5
  conv.bin     encode_ccsds_27_bb's output for 256 data bytes (MSB first),
               from the zero state: 4096 channel bits packed MSB first.
               GNU Radio sends G2 (133 octal) before G1 (171) and doesn't
               invert either.
"""
import ctypes
import gc
import os
import sys

from gnuradio import blocks, fec, gr

lib = ctypes.CDLL("libgnuradio-fec.so")


class Lcg:
    def __init__(self):
        self.x = 1

    def bytes(self, n):
        out = bytearray()
        for _ in range(n):
            self.x = (1103515245 * self.x + 12345) & 0xFFFFFFFF
            out.append((self.x >> 16) & 0xFF)
        return out


def rs(data, pad):
    # Shortening by `pad` = `pad` leading zeros (zero is zero in either
    # basis). libfec's own `pad` argument isn't used: GNU Radio's copy reads
    # 223 bytes whatever it is.
    assert len(data) == 223 - pad
    full = bytes(pad) + bytes(data)
    d = (ctypes.c_ubyte * 223)(*full)
    p = (ctypes.c_ubyte * 32)()
    lib.encode_rs_ccsds(d, p, 0)
    return bytes(data) + bytes(p)


def interleaved(lcg, depth):
    k = 223
    data = lcg.bytes(k * depth)
    block = bytearray(255 * depth)
    for i in range(depth):
        cw = rs(data[i::depth], 0)
        for j, b in enumerate(cw):
            block[j * depth + i] = b
    return bytes(block)


def main():
    gc.disable()  # see the end of the file
    out = sys.argv[1] if len(sys.argv) > 1 else "crates/foxygnal-blocks/tests/fixtures/ccsds"
    os.makedirs(out, exist_ok=True)
    lcg = Lcg()
    i1 = b"".join(rs(lcg.bytes(223), 0) for _ in range(3))
    i1 += rs(lcg.bytes(223 - 33), 33) + rs(lcg.bytes(223 - 95), 95)
    files = {"rs-i1.bin": i1, "rs-i4.bin": interleaved(lcg, 4), "rs-i5.bin": interleaved(lcg, 5)}

    data = lcg.bytes(256)
    tb = gr.top_block()
    src = blocks.vector_source_b(list(data), False)
    snk = blocks.vector_sink_b()
    tb.connect(src, fec.encode_ccsds_27_bb(), snk)
    tb.run()
    bits = snk.data()
    assert len(bits) == 256 * 16
    packed = bytearray()
    for i in range(0, len(bits), 8):
        v = 0
        for b in bits[i : i + 8]:
            v = (v << 1) | (b & 1)
        packed.append(v)
    files["conv.bin"] = bytes(packed)

    for name, blob in files.items():
        with open(os.path.join(out, name), "wb") as f:
            f.write(blob)
        print(f"{name}: {len(blob)} bytes")


if __name__ == "__main__":
    main()
    sys.stdout.flush()
    # GNU Radio 3.10 with Python 3.14 can crash in the garbage collector at
    # exit; everything is written by now.
    os._exit(0)
