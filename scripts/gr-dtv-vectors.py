#!/usr/bin/env python3
"""Known-answer vectors from GNU Radio's gr-dtv for the DVB-S / DVB-T tests.

    scripts/gr-dtv-vectors.py [OUT_DIR]

Writes small binary files (default: crates/foxygnal-blocks/tests/fixtures/gr-dtv)
that `foxygnal-blocks` checks its FEC chain and DVB-T frame against, so CI
doesn't need GNU Radio. Made with GNU Radio 3.10.12. Every vector starts from
the same transport stream, which the Rust tests rebuild: packet p is 0x47 then
187 bytes of the LCG x ← 1103515245·x + 12345 (mod 2³², seed 1, continuing
across packets), each byte bits 16…23 of the new x.

gr-dtv's byte conventions (what the files hold):
- the inner coder's output and the inner interleaver's are one byte per QPSK
  symbol / OFDM data carrier holding v bits, the first (y0) the most
  significant;
- dvbt_map and dvbs2_modulator_bc take such a byte as the point index.

Bit-packed files hold their v-bit values back to back, most significant bit
first. Files (little-endian):
  outer.bin         TS → energy dispersal → RS(204,188) → Forney interleaver:
                    the RS output then the interleaver output, NPKT × 204 each
  inner-R.bin       the inner coder's first 3024 QPSK symbols at rate R
                    (1_2 … 7_8), 2 bits each
  interleave-M-C.bin  the inner (bit + symbol) interleaver's first 2 OFDM
                    symbols for mode M, constellation C, rate 1/2, v bits per
                    data carrier in carrier order
  map-C.bin         dvbt_map's point (2 × f32) for each index 0 … 2^v − 1
  dvbs-qpsk.bin     dvbs2_modulator_bc's QPSK point for 0 … 3
  pilots-M.bin      dvbt_reference_signals with all data carriers 0, 2k 16-QAM
                    3/4 1/8 cell id 0x1234 (8k: 64-QAM 2/3 1/32 cell id
                    0xBEEF): symbols 0 … 3, every carrier 0 … kmax as 4 bits,
                    8 + round(3 × value / TPS level) (±4 pilots, ±3 TPS, 0 data)
  tps.txt           the 68 TPS bits (s0 … s67) of the 4 frames of a
                    superframe, one line each, for a few parameter sets
  ofdm-2k.bin       the whole transmitter's first 2 symbols (with the guard),
                    2k 64-QAM 2/3 1/32 cell id 0x1234, as i16 pairs scaled to
                    a peak of 30000
"""
import os
import sys

import numpy as np
from gnuradio import blocks, digital, dtv, gr

NPKT = 24
MODES = {"2k": (dtv.T2k, 2048, 1512, 1705), "8k": (dtv.T8k, 8192, 6048, 6817)}
CONS = {"qpsk": (dtv.MOD_QPSK, 2), "16qam": (dtv.MOD_16QAM, 4), "64qam": (dtv.MOD_64QAM, 6)}
RATES = {"1_2": dtv.C1_2, "2_3": dtv.C2_3, "3_4": dtv.C3_4, "5_6": dtv.C5_6, "7_8": dtv.C7_8}
GUARDS = {"1/4": (dtv.GI_1_4, 4), "1/8": (dtv.GI_1_8, 8), "1/16": (dtv.GI_1_16, 16), "1/32": (dtv.GI_1_32, 32)}


def ts_packets(n=NPKT):
    x = 1
    out = bytearray()
    for _ in range(n):
        out.append(0x47)
        for _ in range(187):
            x = (1103515245 * x + 12345) & 0xFFFFFFFF
            out.append((x >> 16) & 0xFF)
    return bytes(out)


def pack(vals, v):
    """v-bit values, most significant bit first, as one bit string (MSB first)."""
    bits = (vals[:, None] >> np.arange(v - 1, -1, -1)) & 1
    return np.packbits(bits.astype(np.uint8).reshape(-1))


def run(chain, vlen, cplx=False):
    tb = gr.top_block()
    sink = blocks.vector_sink_c(vlen) if cplx else blocks.vector_sink_b(vlen)
    tb.connect(*chain, sink)
    tb.run()
    return np.array(sink.data())


def outer(npkt=NPKT):
    return [
        # gr-dtv's energy dispersal holds back the last group of 8.
        blocks.vector_source_b(list(ts_packets(npkt + 8)), False),
        dtv.dvbt_energy_dispersal(1),
        dtv.dvbt_reed_solomon_enc(2, 8, 0x11D, 255, 239, 8, 51, 8),
    ]


def coded(npkt, nd, cons, rate):
    return outer(npkt) + [
        dtv.dvbt_convolutional_interleaver(136, 12, 17),
        dtv.dvbt_inner_coder(1, nd, cons, dtv.NH, rate),
    ]


def ofdm(mode, cons, rate, guard, cell_id, data=None, npkt=NPKT):
    """Up to the reference signals (frequency domain → IFFT inside gr-dtv)."""
    tm, n, nd, _ = MODES[mode]
    c, _ = CONS[cons]
    gi, _ = GUARDS[guard]
    if data is None:
        head = coded(npkt, nd, c, rate) + [
            dtv.dvbt_bit_inner_interleaver(nd, c, dtv.NH, tm),
            dtv.dvbt_symbol_inner_interleaver(nd, tm, 1),
            dtv.dvbt_map(nd, c, dtv.NH, tm, 1),
        ]
    else:
        head = [data]
    return head + [
        dtv.dvbt_reference_signals(gr.sizeof_gr_complex, nd, n, c, dtv.NH, rate, dtv.C1_2, gi, tm, 1, cell_id),
    ]


def zero_data_symbols(mode, cons, rate, guard, cell_id, nsym):
    """Frequency-domain carriers 0 … kmax of the first `nsym` symbols with
    every data carrier 0."""
    tm, n, nd, k = MODES[mode]
    src = blocks.vector_source_c([0j] * (nd * nsym), False, nd)
    t = run(ofdm(mode, cons, rate, guard, cell_id, data=src), n, True)
    sym = t.reshape(-1, n)[:nsym]
    f = np.fft.fftshift(np.fft.fft(sym, axis=1), axes=1)
    # Carrier c sits at bin c − kmax/2 (centre carrier at DC).
    return f[:, n // 2 - (k - 1) // 2 : n // 2 + (k - 1) // 2 + 1]


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(root, "crates/foxygnal-blocks/tests/fixtures/gr-dtv")
    os.makedirs(out, exist_ok=True)

    def save(name, data):
        path = os.path.join(out, name)
        if isinstance(data, str):
            open(path, "w").write(data)
        else:
            data.tofile(path)
        print(f"{name}: {os.path.getsize(path)} bytes")

    # Outer coder: RS output, then the interleaver's.
    rs = run(outer(), 1632).astype(np.uint8).reshape(-1)[: NPKT * 204]
    il = run(outer() + [dtv.dvbt_convolutional_interleaver(136, 12, 17)], 1).astype(np.uint8)[: NPKT * 204]
    assert len(rs) == len(il) == NPKT * 204, (len(rs), len(il))
    save("outer.bin", np.concatenate([rs, il]))

    # Inner coder, every rate (QPSK: two bits per byte, I first in DVB-S).
    for name, r in RATES.items():
        sym = run(coded(NPKT, 1512, dtv.MOD_QPSK, r), 1512).astype(np.uint8)
        save(f"inner-{name}.bin", pack(sym.reshape(-1)[:3024], 2))

    # Inner interleaver: two symbols (even, odd).
    for mode, cs in (("2k", ("qpsk", "16qam", "64qam")), ("8k", ("64qam",))):
        tm, n, nd, _ = MODES[mode]
        for cn in cs:
            c, _ = CONS[cn]
            npkt = 4 * nd * CONS[cn][1] // 2 // 8 // 188 + 16
            chain = coded(npkt, nd, c, dtv.C1_2) + [
                dtv.dvbt_bit_inner_interleaver(nd, c, dtv.NH, tm),
                dtv.dvbt_symbol_inner_interleaver(nd, tm, 1),
            ]
            y = run(chain, nd).astype(np.uint8).reshape(-1, nd)[:2]
            assert len(y) == 2
            save(f"interleave-{mode}-{cn}.bin", pack(y.reshape(-1), CONS[cn][1]))

    # Constellations.
    for cn, (c, v) in CONS.items():
        idx = [i % (1 << v) for i in range(1512)]
        pts = run([blocks.vector_source_b(idx, False, 1512), dtv.dvbt_map(1512, c, dtv.NH, dtv.T2k, 1)], 1512, True)
        save(f"map-{cn}.bin", pts[: 1 << v].astype(np.complex64))
    pts = run(
        [
            blocks.vector_source_b([i % 4 for i in range(64800 // 2)], False),
            dtv.dvbs2_modulator_bc(dtv.FECFRAME_NORMAL, dtv.C1_4, dtv.MOD_QPSK, dtv.INTERPOLATION_OFF),
        ],
        1,
        True,
    )
    save("dvbs-qpsk.bin", pts[:4].astype(np.complex64))

    # Pilots and TPS carriers.
    for mode, args in (("2k", ("16qam", dtv.C3_4, "1/8", 0x1234)), ("8k", ("64qam", dtv.C2_3, "1/32", 0xBEEF))):
        f = zero_data_symbols(mode, *args, 4)
        tps_level = np.abs(f[0, 34])  # carrier 34 is a TPS carrier
        assert np.max(np.abs(f.imag)) < 1e-3 * tps_level
        q = np.round(3 * f.real / tps_level)
        assert np.max(np.abs(3 * f.real / tps_level - q)) < 0.01
        save(f"pilots-{mode}.bin", pack((q.reshape(-1) + 8).astype(np.uint8), 4))

    # TPS: differential bits on carrier 34 over a superframe.
    lines = []
    for mode, cn, rn, guard, cell in (
        ("2k", "16qam", "3_4", "1/8", 0x1234),
        ("2k", "qpsk", "1_2", "1/4", 0x0000),
        ("8k", "64qam", "7_8", "1/32", 0xBEEF),
        ("8k", "16qam", "5_6", "1/16", 0x00A5),
    ):
        f = zero_data_symbols(mode, cn, RATES[rn], guard, cell, 4 * 68)
        s = np.sign(f[:, 34].real)
        bits = (s[1:] != s[:-1]).astype(int).reshape(-1)
        bits = np.concatenate([[0], bits]).reshape(4, 68)
        for fr in range(4):
            bits[fr, 0] = 0  # s0 is the reference, not a bit
            lines.append(f"{mode} {cn} {rn.replace('_', '/')} {guard} {cell} {fr} " + "".join(map(str, bits[fr])))
    save("tps.txt", "\n".join(lines) + "\n")

    # The whole transmitter, two symbols in the time domain.
    n, g = 2048, 64
    t = run(ofdm("2k", "64qam", dtv.C2_3, "1/32", 0x1234, npkt=40) + [digital.ofdm_cyclic_prefixer(n, n + g, 0, "")], 1, True)
    t = t[: 2 * (n + g)]
    t = t * (30000 / np.max(np.abs(np.concatenate([t.real, t.imag]))))
    save("ofdm-2k.bin", np.round(np.stack([t.real, t.imag], axis=1)).astype(np.int16).reshape(-1))


if __name__ == "__main__":
    main()
