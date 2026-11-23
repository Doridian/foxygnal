//! The transmit side of [`super::deframer`]: codeblocks in, symbols out
//! (Reed-Solomon parity, randomizer, ASM, NRZ-M, convolutional code, BPSK or
//! QPSK). Used by `ccsds_modulator` and by the tests.

use std::f32::consts::FRAC_1_SQRT_2;

use foxygnal_core::Complex32;

use super::conv::ConvEncoder;
use super::deframer::Config;
use super::randomizer;
use super::rs::Codec;
use super::sync::Modulation;
use super::wire::ASM_BYTES;

pub struct Encoder {
    cfg: Config,
    codec: Option<Codec>,
    conv: ConvEncoder,
    nrzm_level: u8,
    block: Vec<u8>,
    /// An I bit waiting for its Q bit (QPSK).
    half: Option<u8>,
}

impl Encoder {
    pub fn new(cfg: Config) -> Result<Self, String> {
        let codec = cfg.codec()?;
        Ok(Self {
            codec,
            conv: ConvEncoder::new(cfg.conv_invert),
            nrzm_level: 0,
            block: vec![0; cfg.frame_len],
            half: None,
            cfg,
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Channel symbols per frame (ASM included).
    pub fn symbols_per_frame(&self) -> usize {
        self.cfg.frame_channel_bits() / self.cfg.modulation.bits_per_symbol()
    }

    /// Encode one codeblock (`frame_len` bytes: the transfer frame, then room
    /// for the RS parity, which is computed here when RS is on).
    pub fn encode(&mut self, frame: &[u8], mut out: impl FnMut(Complex32)) {
        assert_eq!(frame.len(), self.cfg.frame_len);
        self.block.copy_from_slice(frame);
        if let Some(c) = &self.codec {
            c.encode(&mut self.block);
        }
        if self.cfg.randomized {
            randomizer::apply(&mut self.block);
        }
        let block = std::mem::take(&mut self.block);
        for &byte in ASM_BYTES.iter().chain(&block) {
            for i in (0..8).rev() {
                let mut bit = (byte >> i) & 1;
                if self.cfg.nrzm {
                    self.nrzm_level ^= bit;
                    bit = self.nrzm_level;
                }
                if self.cfg.coded {
                    let [a, b] = self.conv.push(bit);
                    self.channel_bit(a, &mut out);
                    self.channel_bit(b, &mut out);
                } else {
                    self.channel_bit(bit, &mut out);
                }
            }
        }
        self.block = block;
    }

    #[inline]
    fn channel_bit(&mut self, bit: u8, out: &mut impl FnMut(Complex32)) {
        let level = |b: u8| if b == 0 { 1.0 } else { -1.0 };
        match self.cfg.modulation {
            Modulation::Bpsk => out(Complex32::new(level(bit), 0.0)),
            Modulation::Qpsk => match self.half.take() {
                None => self.half = Some(bit),
                Some(i) => out(Complex32::new(
                    level(i) * FRAC_1_SQRT_2,
                    level(bit) * FRAC_1_SQRT_2,
                )),
            },
        }
    }
}
