#!/usr/bin/env python3
"""Make the ATSC known-answer fixtures with GNU Radio's gr-dtv.

    scripts/atsc-fixtures.py [OUT_DIR]

OUT_DIR defaults to crates/foxygnal-blocks/tests/fixtures. Needs GNU Radio
3.10 with gr-dtv. Runs gr-dtv's ATSC transmitter chain (atsc_pad,
atsc_randomizer, atsc_rs_encoder, atsc_interleaver, atsc_trellis_encoder,
atsc_field_sync_mux) on a deterministic transport stream, the same one
`tests/atsc.rs` builds (`fixture_packets`), and writes:

- `atsc_grdtv_symbols.bin`: the first 97 segments (the first field sync and
  96 data segments) of 8-level symbols, 832 per segment, each symbol its
  3-bit index (0 = -7 ... 7 = +7), packed MSB first.
- `atsc_grdtv.txt`: `segments=N fnv64=H` for the first two whole fields
  (626 segments), FNV-1a 64 over one byte per symbol index.

So CI checks our modulator's symbols and our decoder against gr-dtv without
GNU Radio installed.
"""
import os
import sys

import numpy as np
from gnuradio import blocks, dtv, gr

SEGMENTS_RAW = 97
SEGMENTS_HASH = 626


def fixture_packets(n):
    out = bytearray()
    for i in range(n):
        out += bytes([0x47, 0x01, 0x00, 0x10 | (i & 15)])
        out += bytes(((i * 31) ^ (k * 97)) & 0xFF for k in range(4, 188))
    return bytes(out)


def symbols(packets):
    tb = gr.top_block()
    src = blocks.vector_source_b(list(packets), False)
    pad = dtv.atsc_pad()
    rnd = dtv.atsc_randomizer()
    rs = dtv.atsc_rs_encoder()
    il = dtv.atsc_interleaver()
    tr = dtv.atsc_trellis_encoder()
    fsm = dtv.atsc_field_sync_mux()
    v2s = blocks.vector_to_stream(1, 1024)
    # atsc_data_segment: 4 bytes of pipeline info, 832 symbols, padding.
    keep = blocks.keep_m_in_n(1, 832, 1024, 4)
    sink = blocks.vector_sink_b()
    tb.connect(src, pad, rnd, rs, il, tr, fsm, v2s, keep, sink)
    tb.run()
    return np.array(sink.data(), dtype=np.uint8)


def fnv64(data):
    h = 0xCBF29CE484222325
    for b in data:
        h ^= int(b)
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        os.path.dirname(__file__), '..', 'crates', 'foxygnal-blocks', 'tests', 'fixtures')
    os.makedirs(out, exist_ok=True)
    # Two fields plus a margin: the trellis encoder works on 12 segments at a time.
    sym = symbols(fixture_packets(2 * 312 + 24))
    assert len(sym) >= SEGMENTS_HASH * 832, len(sym)
    assert sym.max() < 8

    raw = sym[:SEGMENTS_RAW * 832]
    bits = np.unpackbits(raw[:, None], axis=1)[:, 5:].reshape(-1)
    with open(os.path.join(out, 'atsc_grdtv_symbols.bin'), 'wb') as f:
        f.write(np.packbits(bits).tobytes())
    h = fnv64(sym[:SEGMENTS_HASH * 832].tobytes())
    with open(os.path.join(out, 'atsc_grdtv.txt'), 'w') as f:
        f.write(f'segments={SEGMENTS_HASH} fnv64={h:016x}\n')
    print(f'{len(sym) // 832} segments from gr-dtv; fnv64 of {SEGMENTS_HASH}: {h:016x}')


if __name__ == '__main__':
    main()
