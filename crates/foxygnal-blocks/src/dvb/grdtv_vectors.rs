//! Known-answer tests against GNU Radio's gr-dtv (3.10.12), stage by stage.
//!
//! The vectors in `tests/fixtures/gr-dtv` come from `scripts/gr-dtv-vectors.py`
//! (see its header for the file formats and gr-dtv's byte conventions); they
//! all start from the same transport stream, rebuilt here by [`ts_packets`].
//! End to end, both directions (gr-dtv's transmitter into our demodulators
//! and ours into gr-dtv's receiver) were checked byte-exact with
//! `scripts/dvb-interop.py`; these tests keep the conventions from drifting.

use super::conv::{CodeRate, ConvEncoder};
use super::dvbs::qpsk;
use super::dvbt::frame::{
    Constellation, FRAME_SYMBOLS, Guard, Mode, PILOT_BOOST, Tps, map, tables,
};
use super::outer::OuterEncoder;
use super::{RS_LEN, TS_LEN, rs, scramble};

const NPKT: usize = 24;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/gr-dtv/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The script's test stream: 0x47, then 187 LCG bytes per packet.
fn ts_packets(n: usize) -> Vec<[u8; TS_LEN]> {
    let mut x: u32 = 1;
    (0..n)
        .map(|_| {
            let mut p = [0u8; TS_LEN];
            p[0] = 0x47;
            for b in &mut p[1..] {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                *b = (x >> 16) as u8;
            }
            p
        })
        .collect()
}

/// `v`-bit values packed back to back, most significant bit first.
fn unpack(bytes: &[u8], v: usize) -> Vec<u8> {
    let bit = |i: usize| (bytes[i / 8] >> (7 - i % 8)) & 1;
    (0..bytes.len() * 8 / v)
        .map(|k| (0..v).fold(0, |acc, j| (acc << 1) | bit(k * v + j)))
        .collect()
}

/// The coded bits (one per byte) of `n` packets at `rate`.
fn coded_bits(n: usize, rate: CodeRate) -> Vec<u8> {
    let mut outer = OuterEncoder::new();
    let mut conv = ConvEncoder::new(rate);
    let mut coded = [0u8; RS_LEN];
    let mut bits = Vec::new();
    for p in ts_packets(n) {
        outer.encode(&p, &mut coded);
        for &b in &coded {
            conv.push_byte(b, |x| bits.push(x));
        }
    }
    bits
}

/// Groups of `v` bits as gr-dtv's bytes (first bit most significant).
fn to_symbols(bits: &[u8], v: usize) -> Vec<u8> {
    bits.chunks_exact(v)
        .map(|c| c.iter().fold(0, |acc, &b| (acc << 1) | b))
        .collect()
}

#[test]
fn outer_coder_matches_gr_dtv() {
    let f = fixture("outer.bin");
    let (rs_ref, il_ref) = f.split_at(NPKT * RS_LEN);
    let mut outer = OuterEncoder::new();
    for (i, p) in ts_packets(NPKT).iter().enumerate() {
        // Energy dispersal + RS alone (EN 300 421 §4.4.1, §4.4.2).
        let mut s = *p;
        scramble::scramble(&mut s, i % scramble::GROUP);
        let mut cw = [0u8; RS_LEN];
        cw[..TS_LEN].copy_from_slice(&s);
        rs::encode(&mut cw);
        assert_eq!(cw, rs_ref[i * RS_LEN..(i + 1) * RS_LEN], "RS packet {i}");
        // Plus the Forney interleaver (§4.4.3), zero-filled at the start.
        let mut out = [0u8; RS_LEN];
        outer.encode(p, &mut out);
        assert_eq!(
            out,
            il_ref[i * RS_LEN..(i + 1) * RS_LEN],
            "interleaved packet {i}"
        );
    }
}

#[test]
fn inner_coder_matches_gr_dtv() {
    // The punctured code and the order the kept bits go out in (EN 300 421
    // table 2, EN 300 744 table 1): for DVB-S the first of each pair is I.
    for (rate, name) in CodeRate::ALL
        .into_iter()
        .zip(["1_2", "2_3", "3_4", "5_6", "7_8"])
    {
        let want = unpack(&fixture(&format!("inner-{name}.bin")), 2);
        let got = to_symbols(&coded_bits(NPKT, rate), 2);
        assert_eq!(&got[..want.len()], &want[..], "rate {}", rate.name());
    }
}

#[test]
fn dvbs_qpsk_matches_gr_dtv() {
    // gr-dtv's DVB-S example maps the inner coder's (I, Q) pairs with its
    // DVB-S2 QPSK table; EN 300 421 figure 5 is the same: 0 → +.
    let f = fixture("dvbs-qpsk.bin");
    for idx in 0..4u8 {
        let k = idx as usize * 8;
        let re = f32::from_le_bytes(f[k..k + 4].try_into().unwrap());
        let im = f32::from_le_bytes(f[k + 4..k + 8].try_into().unwrap());
        let p = qpsk(idx >> 1, idx & 1);
        assert!(
            (p.re - re).abs() < 1e-6 && (p.im - im).abs() < 1e-6,
            "{idx}: {p} vs {re}{im:+}j"
        );
    }
}

#[test]
fn inner_interleaver_matches_gr_dtv() {
    // Bit demultiplexing and the six bit interleavers (EN 300 744 §4.3.4.1),
    // the symbol interleaver H(q), even and odd symbols (§4.3.4.2).
    for (mode, cons, name) in [
        (Mode::K2, Constellation::Qpsk, "2k-qpsk"),
        (Mode::K2, Constellation::Qam16, "2k-16qam"),
        (Mode::K2, Constellation::Qam64, "2k-64qam"),
        (Mode::K8, Constellation::Qam64, "8k-64qam"),
    ] {
        let v = cons.bits();
        let want = unpack(&fixture(&format!("interleave-{name}.bin")), v);
        let t = tables(mode);
        let perm = &t.inner(cons).perm;
        let sym_bits = mode.data_carriers() * v;
        let npkt = 2 * sym_bits / 2 / (RS_LEN * 8) + 1;
        let bits = coded_bits(npkt, CodeRate::R1_2);
        let mut carrier_bits = vec![0u8; sym_bits];
        for l in 0..2 {
            for (&dst, &b) in perm[l % 2].iter().zip(&bits[l * sym_bits..]) {
                carrier_bits[dst as usize] = b;
            }
            let got = to_symbols(&carrier_bits, v);
            let nd = mode.data_carriers();
            assert_eq!(got, want[l * nd..(l + 1) * nd], "{name} symbol {l}");
        }
    }
}

#[test]
fn mapping_matches_gr_dtv() {
    // Gray mapping (EN 300 744 §4.3.5, figure 9): y0 the sign of I, y1 of Q,
    // then the amplitude bits; gr-dtv's index has y0 as its top bit.
    for (cons, name) in [
        (Constellation::Qpsk, "qpsk"),
        (Constellation::Qam16, "16qam"),
        (Constellation::Qam64, "64qam"),
    ] {
        let v = cons.bits();
        let f = fixture(&format!("map-{name}.bin"));
        for idx in 0..1usize << v {
            let y: Vec<u8> = (0..v).map(|e| ((idx >> (v - 1 - e)) & 1) as u8).collect();
            let (re, im) = map(cons, &y);
            let k = idx * 8;
            let wre = f32::from_le_bytes(f[k..k + 4].try_into().unwrap());
            let wim = f32::from_le_bytes(f[k + 4..k + 8].try_into().unwrap());
            assert!(
                (re - wre).abs() < 1e-5 && (im - wim).abs() < 1e-5,
                "{name} {idx:0v$b}: {re}{im:+}j vs {wre}{wim:+}j"
            );
        }
    }
}

fn tps(mode: Mode, cons: Constellation, rate: CodeRate, guard: Guard, cell: u16, f: u8) -> Tps {
    Tps {
        frame: f,
        constellation: cons,
        hierarchy: 0,
        rate_hp: rate,
        rate_lp: CodeRate::R1_2,
        guard,
        mode,
        cell_id_byte: if f.is_multiple_of(2) {
            (cell >> 8) as u8
        } else {
            cell as u8
        },
    }
}

#[test]
fn tps_matches_gr_dtv() {
    // s1…s67 of a superframe (EN 300 744 §4.6.2): sync word, length
    // indicator 011111 (cell id sent), frame number, parameters, the cell
    // id's high byte in frames 1 and 3, and BCH(67,53).
    let text = String::from_utf8(fixture("tps.txt")).unwrap();
    let mut lines = 0;
    for line in text.lines() {
        let w: Vec<&str> = line.split(' ').collect();
        let t = tps(
            Mode::parse(w[0]).unwrap(),
            Constellation::parse(w[1]).unwrap(),
            CodeRate::parse(w[2]).unwrap(),
            Guard::parse(w[3]).unwrap(),
            w[4].parse().unwrap(),
            w[5].parse().unwrap(),
        );
        let want: Vec<u8> = w[6].bytes().map(|b| b - b'0').collect();
        assert_eq!(want.len(), FRAME_SYMBOLS);
        assert_eq!(&t.bits()[1..], &want[1..], "{line}");
        lines += 1;
    }
    assert_eq!(lines, 16);
}

#[test]
fn pilots_and_tps_carriers_match_gr_dtv() {
    // Continual and scattered pilots (§4.5.3, §4.5.4: 3·(l mod 4) + 12p),
    // their PRBS values (§4.5.2: x¹¹ + x² + 1 from all ones, ±4/3), and the
    // TPS carriers (DBPSK, symbol 0 from the PRBS), symbols 0…3.
    for (mode, cons, rate, guard, cell, name) in [
        (
            Mode::K2,
            Constellation::Qam16,
            CodeRate::R3_4,
            Guard::G1_8,
            0x1234,
            "2k",
        ),
        (
            Mode::K8,
            Constellation::Qam64,
            CodeRate::R2_3,
            Guard::G1_32,
            0xBEEF,
            "8k",
        ),
    ] {
        let want: Vec<i8> = unpack(&fixture(&format!("pilots-{name}.bin")), 4)
            .into_iter()
            .map(|x| x as i8 - 8)
            .collect();
        let layout = &tables(mode).layout;
        let k = layout.k;
        let bits = tps(mode, cons, rate, guard, cell, 0).bits();
        let mut tps_state: Vec<f32> = layout
            .tps
            .iter()
            .map(|&c| layout.prbs[c as usize])
            .collect();
        for l in 0..4 {
            let mut got = vec![0f32; k];
            for &c in &layout.continual {
                got[c as usize] = PILOT_BOOST * layout.prbs[c as usize];
            }
            for c in (3 * (l % 4)..k).step_by(12) {
                got[c] = PILOT_BOOST * layout.prbs[c];
            }
            for (i, &c) in layout.tps.iter().enumerate() {
                if l > 0 && bits[l] == 1 {
                    tps_state[i] = -tps_state[i];
                }
                got[c as usize] = tps_state[i];
            }
            for c in 0..k {
                let g = (3.0 * got[c]).round() as i8;
                assert_eq!(g, want[l * k + c], "{name} symbol {l} carrier {c}");
            }
        }
    }
}
