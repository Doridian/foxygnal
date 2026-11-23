#!/usr/bin/env python3
"""End-to-end DVB-T / DVB-S interop with GNU Radio's gr-dtv, both directions.

    cargo build --release -p foxygnal
    scripts/dvb-interop.py [dvbt-rx|dvbt-tx|dvbs-rx|dvbs-tx|all] [--quick]

dvbt-rx   gr-dtv's DVB-T transmitter (as examples/dtv/dvbt_tx_8k.grc) → c32 at
          64/7 MS/s → `dvbt_demodulator`; also our modulator against gr-dtv's,
          symbol by symbol in the frequency domain
dvbt-tx   `dvbt_modulator` → gr-dtv's receiver (as dvbt_rx_8k.grc)
dvbs-rx   gr-dtv's DVB-S chain (as dvbs_tx.grc: the DVB-T outer and inner
          coders, dvbs2_modulator_bc QPSK, RRC 0.35) at 2 samples/symbol →
          `dvbs_demodulator`
dvbs-tx   `dvbs_modulator` → RRC matched filter and hard decisions here →
          gr-dtv's dvbt_viterbi_decoder (QPSK), deinterleaver, RS, descrambler

Each case sends random TS packets (PID 0x100) and checks the TS that comes out
is byte-exact. Needs GNU Radio 3.10 with gr-dtv and numpy. Doesn't touch any
radio hardware. The known-answer vectors the unit tests use come from
scripts/gr-dtv-vectors.py.
"""
import os
import subprocess
import sys
import tempfile

import numpy as np
from gnuradio import blocks, digital, dtv, fft, filter, gr
from gnuradio.fft import window
from gnuradio.filter import firdes

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FOX = os.environ.get("FOXYGNAL", os.path.join(ROOT, "target/release/foxygnal"))
MODES = {"2k": (dtv.T2k, 2048, 1512, 1705), "8k": (dtv.T8k, 8192, 6048, 6817)}
CONS = {"qpsk": dtv.MOD_QPSK, "16qam": dtv.MOD_16QAM, "64qam": dtv.MOD_64QAM}
RATES = {"1/2": dtv.C1_2, "2/3": dtv.C2_3, "3/4": dtv.C3_4, "5/6": dtv.C5_6, "7/8": dtv.C7_8}
GUARDS = {"1/4": (dtv.GI_1_4, 4), "1/8": (dtv.GI_1_8, 8), "1/16": (dtv.GI_1_16, 16), "1/32": (dtv.GI_1_32, 32)}
CELL = 0x1234

# Every constellation × rate in 2k (guards cycling), one 8k per constellation.
FULL = []
for ci, c in enumerate(CONS):
    for ri, r in enumerate(RATES):
        FULL.append(("2k", c, r, list(GUARDS)[(5 * ci + ri) % 4]))
    FULL.append(("8k", c, "3/4", list(GUARDS)[(5 * ci + 1) % 4]))
QUICK = [("2k", "qpsk", "1/2", "1/4"), ("2k", "16qam", "3/4", "1/8"), ("8k", "64qam", "2/3", "1/32")]


def ts_file(path, n, seed):
    rng = np.random.default_rng(seed)
    p = rng.integers(0, 256, (n, 188), dtype=np.uint8)
    p[:, 0], p[:, 1], p[:, 2] = 0x47, 0x01, 0x00
    p[:, 3] = 0x10 | (np.arange(n) % 16)
    p.tofile(path)


def compare(sent, got_path):
    """(packets compared, mismatched) after aligning on the first good one."""
    ref = np.fromfile(sent, dtype=np.uint8).reshape(-1, 188)
    got = np.fromfile(got_path, dtype=np.uint8)
    got = got[: len(got) // 188 * 188].reshape(-1, 188)
    index = {bytes(p[4:20]): i for i, p in enumerate(ref)}
    for j, p in enumerate(got):
        i = index.get(bytes(p[4:20]))
        if not p[1] & 0x80 and i is not None:
            n = min(len(got) - j, len(ref) - i)
            return n, int(np.any(got[j : j + n] != ref[i : i + n], axis=1).sum())
    return 0, len(got)


def fox(graph, d):
    path = os.path.join(d, "graph.fox.toml")
    open(path, "w").write(graph)
    subprocess.run([FOX, "run", path], cwd=d, check=True, capture_output=True, timeout=900)


def fox_graph(blocks_, conns):
    out = ""
    for bid, kind, params in blocks_:
        out += f'[[block]]\nid = "{bid}"\nkind = "{kind}"\nparams = {{ {params} }}\n\n'
    for a, b in conns:
        out += f'[[connect]]\nfrom = "{a}"\nto = "{b}"\n\n'
    return out


def normalize(path, rms):
    x = np.fromfile(path, dtype=np.complex64)
    (x * (rms / np.sqrt(np.mean(np.abs(x) ** 2)))).astype(np.complex64).tofile(path)


# ---- DVB-T -------------------------------------------------------------------


def gr_dvbt_tx(ts, out, mode, cons, rate, guard):
    tm, n, nd, _ = MODES[mode]
    gi, gdiv = GUARDS[guard]
    c, r = CONS[cons], RATES[rate]
    tb = gr.top_block()
    sink = blocks.file_sink(gr.sizeof_gr_complex, out, False)
    tb.connect(
        blocks.file_source(1, ts, False),
        dtv.dvbt_energy_dispersal(1),
        dtv.dvbt_reed_solomon_enc(2, 8, 0x11D, 255, 239, 8, 51, 8),
        dtv.dvbt_convolutional_interleaver(136, 12, 17),
        dtv.dvbt_inner_coder(1, nd, c, dtv.NH, r),
        dtv.dvbt_bit_inner_interleaver(nd, c, dtv.NH, tm),
        dtv.dvbt_symbol_inner_interleaver(nd, tm, 1),
        dtv.dvbt_map(nd, c, dtv.NH, tm, 1),
        # Non-hierarchical: the LP code rate bits are 000 (EN 300 744 §4.6.2).
        dtv.dvbt_reference_signals(gr.sizeof_gr_complex, nd, n, c, dtv.NH, r, dtv.C1_2, gi, tm, 1, CELL),
        digital.ofdm_cyclic_prefixer(n, n + n // gdiv, 0, ""),
        sink,
    )
    tb.run()
    sink.close()
    normalize(out, 0.25)


def gr_dvbt_rx(inp, out, mode, cons, rate, guard):
    tm, n, nd, k = MODES[mode]
    gi, gdiv = GUARDS[guard]
    c, r = CONS[cons], RATES[rate]
    tb = gr.top_block()
    sink = blocks.file_sink(1, out, False)
    tb.connect(
        blocks.file_source(gr.sizeof_gr_complex, inp, False),
        dtv.dvbt_ofdm_sym_acquisition(1, n, k, n // gdiv, 30),
        fft.fft_vcc(n, True, window.rectangular(n), True, 1),
        dtv.dvbt_demod_reference_signals(gr.sizeof_gr_complex, n, nd, c, dtv.NH, r, dtv.C1_2, gi, tm, 1, 0),
        dtv.dvbt_demap(nd, c, dtv.NH, tm, 1),
        dtv.dvbt_symbol_inner_interleaver(nd, tm, 0),
        dtv.dvbt_bit_inner_deinterleaver(nd, c, dtv.NH, tm),
        blocks.vector_to_stream(1, nd),
        dtv.dvbt_viterbi_decoder(c, dtv.NH, r, 768),
        dtv.dvbt_convolutional_deinterleaver(136, 12, 17),
        dtv.dvbt_reed_solomon_dec(2, 8, 0x11D, 255, 239, 8, 51, 8),
        dtv.dvbt_energy_descramble(8),
        sink,
    )
    tb.run()
    sink.close()


def fox_dvbt_tx(d, mode, cons, rate, guard):
    fox(
        fox_graph(
            [
                ("src", "file_source", 'type = "u8", path = "sent.ts"'),
                ("mod", "dvbt_modulator", f'mode = "{mode}", constellation = "{cons}", rate = "{rate}", '
                 f'guard = "{guard}", bandwidth = "8", cell_id = {CELL}, gain = 0.25'),
                ("out", "file_sink", 'type = "c32", path = "fox.c32"'),
            ],
            [("src.out", "mod.in"), ("mod.out", "out.in")],
        ),
        d,
    )


def fox_rx(d, demod, params, rate):
    fox(
        fox_graph(
            [
                ("src", "file_source", f'type = "c32", path = "air.c32", sample_rate = {rate}'),
                ("demod", demod, params),
                ("ts", "file_sink", 'type = "u8", path = "got.ts"'),
                ("log", "null_sink", 'type = "u8"'),
            ],
            [("src.out", "demod.in"), ("demod.ts", "ts.in"), ("demod.status", "log.in")],
        ),
        d,
    )


def spectra_match(a_path, b_path, n, g):
    """Our modulator vs gr-dtv's, every symbol, up to one complex scale."""
    a = np.fromfile(a_path, dtype=np.complex64)
    b = np.fromfile(b_path, dtype=np.complex64)
    L = n + g
    ns = min(len(a), len(b)) // L
    A = np.fft.fft(a[: ns * L].reshape(ns, L)[:, g:], axis=1)
    B = np.fft.fft(b[: ns * L].reshape(ns, L)[:, g:], axis=1)
    k = A[0] @ B[0].conj() / (B[0] @ B[0].conj())
    bad = sum(np.max(np.abs(A[s] - k * B[s])) > 1e-3 * np.max(np.abs(A[s])) for s in range(ns))
    return ns, bad


def dvbt(direction, cases, d):
    ok = True
    for mode, cons, rate, guard in cases:
        sent = os.path.join(d, "sent.ts")
        ts_file(sent, 12000 if mode == "8k" else 4000, 1)
        if direction == "rx":
            gr_dvbt_tx(sent, os.path.join(d, "air.c32"), mode, cons, rate, guard)
            fox_rx(d, "dvbt_demodulator", 'mode = "auto", guard = "auto", bandwidth = "8"', "9142857.142857143")
            n, bad = compare(sent, os.path.join(d, "got.ts"))
            fox_dvbt_tx(d, mode, cons, rate, guard)
            nfft = MODES[mode][1]
            ns, sbad = spectra_match(os.path.join(d, "fox.c32"), os.path.join(d, "air.c32"), nfft, nfft // GUARDS[guard][1])
            res = f"{n} packets, {bad} bad; tx spectra {ns} symbols, {sbad} differ"
            good = n > 0 and bad == 0 and sbad == 0
        else:
            fox_dvbt_tx(d, mode, cons, rate, guard)
            gr_dvbt_rx(os.path.join(d, "fox.c32"), os.path.join(d, "got.ts"), mode, cons, rate, guard)
            n, bad = compare(sent, os.path.join(d, "got.ts"))
            res = f"{n} packets, {bad} bad"
            good = n > 0 and bad == 0
        ok &= good
        print(f"dvbt-{direction} {mode} {cons} {rate} {guard}: {res} {'ok' if good else 'FAIL'}")
    return ok


# ---- DVB-S -------------------------------------------------------------------


def gr_dvbs_tx(ts, out, rate):
    tb = gr.top_block()
    sink = blocks.file_sink(gr.sizeof_gr_complex, out, False)
    tb.connect(
        blocks.file_source(1, ts, False),
        dtv.dvbt_energy_dispersal(1),
        dtv.dvbt_reed_solomon_enc(2, 8, 0x11D, 255, 239, 8, 51, 8),
        dtv.dvbt_convolutional_interleaver(136, 12, 17),
        dtv.dvbt_inner_coder(1, 1512, dtv.MOD_QPSK, dtv.NH, RATES[rate]),
        blocks.vector_to_stream(1, 1512),
        dtv.dvbs2_modulator_bc(dtv.FECFRAME_NORMAL, dtv.C1_4, dtv.MOD_QPSK, dtv.INTERPOLATION_ON),
        filter.fft_filter_ccc(1, firdes.root_raised_cosine(1.0, 2.0, 1.0, 0.35, 41), 1),
        sink,
    )
    tb.run()
    sink.close()
    normalize(out, 0.5)


def gr_dvbs_decode(syms, rate):
    # gr-dtv's QPSK byte: the I bit (first sent) on top.
    data = (((syms.real < 0) << 1) | (syms.imag < 0)).astype(np.uint8)
    n = len(data) // 1512 * 1512
    tb = gr.top_block()
    sink = blocks.vector_sink_b()
    tb.connect(
        blocks.vector_source_b(data[:n].tolist(), False),
        dtv.dvbt_viterbi_decoder(dtv.MOD_QPSK, dtv.NH, RATES[rate], 768),
        dtv.dvbt_convolutional_deinterleaver(136, 12, 17),
        dtv.dvbt_reed_solomon_dec(2, 8, 0x11D, 255, 239, 8, 51, 8),
        dtv.dvbt_energy_descramble(8),
        sink,
    )
    tb.run()
    return np.array(sink.data(), dtype=np.uint8)


def gr_dvbs_rx(inp, out, rate):
    x = np.fromfile(inp, dtype=np.complex64)
    y = np.convolve(x, np.array(firdes.root_raised_cosine(1.0, 2.0, 1.0, 0.35, 41)))
    s = y[max(range(2), key=lambda p: np.mean(np.abs(y[p::2].real))) :: 2]
    # The first symbol: the start that decodes best (the puncturing period
    # starts there).
    def good(start):
        ts = gr_dvbs_decode(s[start : start + 500000], rate)
        ts = ts[: len(ts) // 188 * 188].reshape(-1, 188)
        return int(np.sum(ts[:, 1] & 0x80 == 0))
    start = max(range(40), key=good)
    gr_dvbs_decode(s[start:], rate).tofile(out)


def dvbs(direction, rates, d):
    ok = True
    for rate in rates:
        sent = os.path.join(d, "sent.ts")
        ts_file(sent, 6000, 3)
        if direction == "rx":
            gr_dvbs_tx(sent, os.path.join(d, "air.c32"), rate)
            fox_rx(d, "dvbs_demodulator", 'symbol_rate = 1000000, rate = "auto"', 2000000)
        else:
            fox(
                fox_graph(
                    [
                        ("src", "file_source", 'type = "u8", path = "sent.ts"'),
                        ("mod", "dvbs_modulator", f'symbol_rate = 1000000, rate = "{rate}", sps = 2, gain = 0.5'),
                        ("out", "file_sink", 'type = "c32", path = "fox.c32"'),
                    ],
                    [("src.out", "mod.in"), ("mod.out", "out.in")],
                ),
                d,
            )
            gr_dvbs_rx(os.path.join(d, "fox.c32"), os.path.join(d, "got.ts"), rate)
        n, bad = compare(sent, os.path.join(d, "got.ts"))
        good = n > 0 and bad == 0
        ok &= good
        print(f"dvbs-{direction} {rate}: {n} packets, {bad} bad {'ok' if good else 'FAIL'}")
    return ok


def main():
    what = [a for a in sys.argv[1:] if not a.startswith("-")] or ["all"]
    quick = "--quick" in sys.argv
    if what == ["all"]:
        what = ["dvbt-rx", "dvbt-tx", "dvbs-rx", "dvbs-tx"]
    cases = QUICK if quick else FULL
    rates = ["1/2", "7/8"] if quick else list(RATES)
    ok = True
    with tempfile.TemporaryDirectory() as d:
        for w in what:
            std, direction = w.split("-")
            ok &= dvbt(direction, cases, d) if std == "dvbt" else dvbs(direction, rates, d)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
