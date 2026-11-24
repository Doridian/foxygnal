#!/usr/bin/env python3
"""Known-answer vectors for Metop AHRPT's rate 3/4 punctured convolutional
code, from scikit-commpy.

    pip install scikit-commpy
    scripts/ahrpt-conv-vectors.py [OUT_FILE]

AHRPT codes the CADU stream with the K = 7 code (G1 = 171 octal, G2 = 133
octal, G2 not inverted) punctured to rate 3/4 with CCSDS 131.0-B's pattern:
G1 kept on the first and third bit of each three, G2 on the first and second,
sent in the order G1(1) G2(1) G2(2) G1(3). commpy's encoder (its own trellis
and puncturing code) makes that serial stream from a seeded random message,
which `polar::ahrpt`'s Puncturer must reproduce and its Depuncturer plus the
Viterbi decoder must decode.

Writes (default crates/foxygnal-blocks/tests/fixtures/ahrpt/conv34.bin):
message length in bits (u32 LE), the message bits packed MSB first, then the
punctured code bits packed MSB first (4 for every 3 message bits).
"""
import os
import sys

import numpy as np
from commpy.channelcoding import Trellis, conv_encode

N = 3 * 4000


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.normpath(os.path.join(
        os.path.dirname(__file__), "..", "crates", "foxygnal-blocks", "tests", "fixtures",
        "ahrpt", "conv34.bin"))
    rng = np.random.default_rng(20261123)
    msg = rng.integers(0, 2, N).astype(int)
    # commpy's generator octals read the other way round from the usual
    # notation (its most significant bit taps the oldest bit), so 171/133
    # go in bit-reversed: 117/155. The impulse response pins that down: it
    # is the generators' taps from the input on, CCSDS 131.0-B's G1 =
    # 1111001 and G2 = 1011011.
    trellis = Trellis(np.array([6]), np.array([[0o117, 0o155]]))
    imp = conv_encode(np.array([1, 0, 0, 0, 0, 0, 0]), trellis, termination="cont")
    assert list(imp[0::2]) == [1, 1, 1, 1, 0, 0, 1], imp
    assert list(imp[1::2]) == [1, 0, 1, 1, 0, 1, 1], imp
    # Unpunctured first: (G1, G2) per bit, starting from the zero state.
    full = conv_encode(msg, trellis, termination="cont").astype(np.uint8)
    assert len(full) == 2 * N
    # commpy punctures the serial stream G1 G2 G1 G2 … with the first row
    # of its matrix as a repeating mask (and pads its output with zeros):
    # over three bits, keep G1(1) G2(1), drop G1(2), keep G2(2) G1(3), drop
    # G2(3).
    punct = conv_encode(msg, trellis, termination="cont",
                        puncture_matrix=np.array([[1, 1, 0, 1, 1, 0]])).astype(np.uint8)
    assert not punct[4 * N // 3:].any()
    punct = punct[:4 * N // 3]
    # Check that against the unpunctured stream in the CCSDS order
    # G1 G2 G2 G1.
    g1, g2 = full[0::2], full[1::2]
    want = np.stack([g1[0::3], g2[0::3], g2[1::3], g1[2::3]], axis=1).reshape(-1)
    assert np.array_equal(punct, want)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "wb") as f:
        f.write(N.to_bytes(4, "little"))
        f.write(np.packbits(msg.astype(np.uint8)).tobytes())
        f.write(np.packbits(punct).tobytes())
    print(f"{out}: {N} message bits, {len(punct)} code bits")


if __name__ == "__main__":
    main()
