//! Known-answer tests against independent implementations: Phil Karn's
//! libfec RS(255,223) (`encode_rs_ccsds`, dual basis) and GNU Radio's
//! `encode_ccsds_27_bb`, both from GNU Radio 3.10.12's libgnuradio-fec. The
//! vectors in `tests/fixtures/ccsds` come from `scripts/ccsds-vectors.py`
//! (see its header for the formats); the data are its LCG bytes, rebuilt
//! here by [`Lcg`].

use super::conv::{ConvDecoder, ConvEncoder};
use super::rs::Codec;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/ccsds/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

struct Lcg(u32);

impl Lcg {
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n)
            .map(|_| {
                self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (self.0 >> 16) as u8
            })
            .collect()
    }
}

#[test]
fn reed_solomon_matches_libfec() {
    let mut lcg = Lcg(1);
    let i1 = fixture("rs-i1.bin");
    let mut at = 0;
    for pad in [0usize, 0, 0, 33, 95] {
        let n = 255 - pad;
        let want = &i1[at..at + n];
        at += n;
        let data = lcg.bytes(223 - pad);
        assert_eq!(want[..223 - pad], data[..], "the script's data");
        let c = Codec::new(n, 1, true).unwrap();
        let mut block = vec![0u8; n];
        block[..223 - pad].copy_from_slice(&data);
        c.encode(&mut block);
        assert_eq!(block, want, "shortened by {pad}");
        // And it corrects 16 errors in libfec's codeword.
        let mut bad = want.to_vec();
        for k in 0..16 {
            bad[(k * 13 + pad) % n] ^= 0xA5;
        }
        assert!(c.decode(&mut bad).ok());
        assert_eq!(bad, want);
    }
    assert_eq!(at, i1.len());
    for (name, depth) in [("rs-i4.bin", 4), ("rs-i5.bin", 5)] {
        let want = fixture(name);
        let len = 255 * depth;
        assert_eq!(want.len(), len);
        let data = lcg.bytes(223 * depth);
        let c = Codec::new(len, depth, true).unwrap();
        let mut block = vec![0u8; len];
        block[..223 * depth].copy_from_slice(&data);
        c.encode(&mut block);
        assert_eq!(block, want, "{name}");
    }
}

#[test]
fn convolutional_code_matches_gnu_radio() {
    let mut lcg = Lcg(1);
    // The data follow the RS vectors' in the script.
    lcg.bytes(223 * 3 + (223 - 33) + (223 - 95) + 223 * 4 + 223 * 5);
    let data = lcg.bytes(256);
    let gr = fixture("conv.bin");
    let gr_bit = |k: usize| (gr[k / 8] >> (7 - k % 8)) & 1;
    // GNU Radio: G2 then G1, neither inverted. Ours, without inversion: G1
    // then G2.
    let mut enc = ConvEncoder::new(false);
    let mut k = 0;
    for &byte in &data {
        for i in (0..8).rev() {
            let [g1, g2] = enc.push(byte >> i);
            assert_eq!([g2, g1], [gr_bit(k), gr_bit(k + 1)], "channel bit {k}");
            k += 2;
        }
    }
    // Our CCSDS decoder takes GNU Radio's stream with the pairs swapped and
    // G2 inverted.
    let mut dec = ConvDecoder::new(true);
    let mut bits = Vec::new();
    let mut out = |b: &[u8]| bits.extend_from_slice(b);
    for p in 0..data.len() * 8 {
        let s = |b: u8| if b == 0 { 50i8 } else { -50 };
        dec.push([s(gr_bit(2 * p + 1)), s(gr_bit(2 * p) ^ 1)], &mut out);
    }
    dec.flush(&mut out);
    let got: Vec<u8> = bits
        .chunks(8)
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b))
        .collect();
    assert_eq!(got, data);
}
