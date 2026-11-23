//! The outer part of the DVB FEC chain, shared by DVB-S and DVB-T: energy
//! dispersal, RS(204,188) and the Forney interleaver, plus the packet sync
//! that finds the 204-byte grid in the inner decoder's bit stream.
//!
//! Transmit: [`OuterEncoder`], 188-byte TS packets → 204-byte interleaved
//! packets. Receive: bits → [`TsSync`] → [`OuterDecoder`] → TS packets.

use super::interleave::ConvInterleaver;
use super::scramble::{self, GROUP};
use super::{RS_LEN, SYNC, SYNC_INV, TS_LEN, rs};

/// Bits per interleaved packet.
pub const PACKET_BITS: usize = RS_LEN * 8;

#[inline]
pub fn is_sync(b: u8) -> bool {
    b == SYNC || b == SYNC_INV
}

/// Energy dispersal, RS encoding and interleaving.
pub struct OuterEncoder {
    il: ConvInterleaver,
    group: usize,
}

impl Default for OuterEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl OuterEncoder {
    pub fn new() -> Self {
        Self {
            il: ConvInterleaver::interleaver(),
            group: 0,
        }
    }

    /// Encode one TS packet (its sync byte is replaced).
    pub fn encode(&mut self, ts: &[u8; TS_LEN], out: &mut [u8; RS_LEN]) {
        let mut p = *ts;
        scramble::scramble(&mut p, self.group);
        self.group = (self.group + 1) % GROUP;
        out[..TS_LEN].copy_from_slice(&p);
        rs::encode(out);
        self.il.process(out);
    }
}

/// Deinterleaving, RS decoding and energy dispersal removal, one packet at a
/// time, from packets that start on a sync byte (from [`TsSync`]).
pub struct OuterDecoder {
    dil: ConvInterleaver,
    /// Packets pushed since the last reset (the deinterleaver needs 11).
    filled: usize,
    /// Position of the latest input packet in its energy-dispersal group,
    /// once an inverted sync byte has been seen.
    in_group: Option<usize>,
    buf: [u8; RS_LEN],
    /// Packets output.
    pub packets: u64,
    /// Bytes RS corrected.
    pub corrected: u64,
    /// Packets RS couldn't correct (output with transport_error_indicator set).
    pub failed: u64,
}

impl Default for OuterDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl OuterDecoder {
    pub fn new() -> Self {
        Self {
            dil: ConvInterleaver::deinterleaver(),
            filled: 0,
            in_group: None,
            buf: [0; RS_LEN],
            packets: 0,
            corrected: 0,
            failed: 0,
        }
    }

    /// Start over (after a loss of sync); keeps the counters.
    pub fn reset(&mut self) {
        self.dil.reset();
        self.filled = 0;
        self.in_group = None;
    }

    /// Take one interleaved packet; returns true when `out` holds a decoded
    /// TS packet. Not for the first 11 after a reset, which the deinterleaver
    /// is still filling with, nor before the energy-dispersal group's phase
    /// is known (from an inverted sync byte, at most 8 packets).
    pub fn decode(&mut self, pkt: &[u8; RS_LEN], out: &mut [u8; TS_LEN]) -> bool {
        // Sync bytes go through the interleaver's branch 0 undelayed, so the
        // group phase is visible before deinterleaving.
        self.in_group = if pkt[0] == SYNC_INV {
            Some(0)
        } else {
            self.in_group.map(|g| (g + 1) % GROUP)
        };
        self.buf = *pkt;
        self.dil.process(&mut self.buf);
        if self.filled < super::interleave::I - 1 {
            self.filled += 1;
            return false;
        }
        let ok = match rs::decode(&mut self.buf) {
            Ok(n) => {
                self.corrected += n as u64;
                true
            }
            Err(_) => {
                self.failed += 1;
                false
            }
        };
        // The output is 11 packets behind the input.
        const LAG: usize = (super::interleave::I - 1) % GROUP;
        let mut group = self.in_group.map(|g| (g + GROUP - LAG) % GROUP);
        if ok && self.buf[0] == SYNC_INV && group != Some(0) {
            // A corrected inverted sync byte outranks the count.
            self.in_group = Some(LAG);
            group = Some(0);
        }
        let Some(group) = group else {
            return false;
        };
        out.copy_from_slice(&self.buf[..TS_LEN]);
        scramble::descramble(out, group);
        if !ok {
            // transport_error_indicator.
            out[1] |= 0x80;
        }
        self.packets += 1;
        true
    }
}

/// Bits per hunt history (two packets back).
const HUNT_HIST: usize = 2 * PACKET_BITS;
/// Consecutive wrong sync bytes before the packet grid counts as lost.
const MAX_BAD_SYNC: u32 = 8;

/// Finds the packet grid in a decoded bit stream: three sync bytes (0x47 or
/// 0xB8) 204 bytes apart, at any bit offset. Then packs bytes and hands out
/// whole 204-byte packets starting at a sync byte, until 8 sync bytes in a row
/// are wrong.
///
/// Also handles an inverted stream, which a QPSK receiver locked 180° off
/// produces (the code maps inverted inputs to inverted outputs, and 0x47
/// inverted is 0xB8): only one sync byte in 8 is 0xB8, so two in a row mean
/// the polarity is wrong, at lock or later, after a 180° slip of the carrier
/// loop, which the sync bytes alone can't show.
pub struct TsSync {
    locked: bool,
    win: u8,
    hist: Box<[u8; HUNT_HIST]>,
    hpos: usize,
    invert: u8,
    byte: u8,
    nbits: u32,
    pkt: [u8; RS_LEN],
    fill: usize,
    bad: u32,
    fresh: bool,
    /// The previous packet's sync byte was 0xB8.
    last_inv: bool,
    /// Polarity changes after lock.
    pub flips: u64,
    /// Times the grid was lost.
    pub losses: u64,
}

impl Default for TsSync {
    fn default() -> Self {
        Self::new()
    }
}

impl TsSync {
    pub fn new() -> Self {
        Self {
            locked: false,
            win: 0,
            hist: Box::new([0; HUNT_HIST]),
            hpos: 0,
            invert: 0,
            byte: 0,
            nbits: 0,
            pkt: [0; RS_LEN],
            fill: 0,
            bad: 0,
            fresh: false,
            last_inv: false,
            flips: 0,
            losses: 0,
        }
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// The bit stream was inverted (and is being inverted back).
    pub fn inverted(&self) -> bool {
        self.invert != 0
    }

    /// Forget the grid and hunt again.
    pub fn reset(&mut self) {
        self.locked = false;
        self.win = 0;
        self.hist.fill(0);
        self.hpos = 0;
    }

    /// Take decoded bits (one per byte); `on_packet` gets each whole packet,
    /// and `true` with the first one after the grid was (re)found, when
    /// whatever follows (a deinterleaver) has to start over.
    pub fn push(&mut self, bits: &[u8], mut on_packet: impl FnMut(&[u8; RS_LEN], bool)) {
        let mut i = 0;
        while i < bits.len() {
            if !self.locked {
                i += self.hunt(&bits[i..]);
                continue;
            }
            let b = bits[i];
            i += 1;
            self.byte = (self.byte << 1) | b;
            self.nbits += 1;
            if self.nbits < 8 {
                continue;
            }
            self.nbits = 0;
            let mut v = self.byte ^ self.invert;
            if self.fill == 0 {
                if is_sync(v) {
                    self.bad = 0;
                    if v == SYNC_INV && self.last_inv {
                        self.invert ^= 0xFF;
                        v ^= 0xFF;
                        self.flips += 1;
                    }
                    self.last_inv = v == SYNC_INV;
                } else {
                    self.last_inv = false;
                    self.bad += 1;
                    if self.bad >= MAX_BAD_SYNC {
                        self.reset();
                        self.losses += 1;
                        continue;
                    }
                }
            }
            self.pkt[self.fill] = v;
            self.fill += 1;
            if self.fill == RS_LEN {
                on_packet(&self.pkt, std::mem::take(&mut self.fresh));
                self.fill = 0;
            }
        }
    }

    /// Look for the grid; returns how many bits were used (up to and
    /// including the sync byte that completed it).
    fn hunt(&mut self, bits: &[u8]) -> usize {
        for (i, &b) in bits.iter().enumerate() {
            self.win = (self.win << 1) | b;
            let back2 = self.hist[self.hpos];
            let back1 = self.hist[(self.hpos + PACKET_BITS) % HUNT_HIST];
            self.hist[self.hpos] = self.win;
            self.hpos = (self.hpos + 1) % HUNT_HIST;
            if is_sync(self.win) && is_sync(back1) && is_sync(back2) {
                let inv = [self.win, back1, back2]
                    .iter()
                    .filter(|&&v| v == SYNC_INV)
                    .count();
                // Only one sync byte in 8 is 0xB8, so of three in a row at
                // most one is, unless the stream is inverted.
                self.invert = if inv >= 2 { 0xFF } else { 0 };
                self.locked = true;
                self.pkt[0] = self.win ^ self.invert;
                self.fill = 1;
                self.nbits = 0;
                self.byte = 0;
                self.bad = 0;
                self.fresh = true;
                self.last_inv = self.pkt[0] == SYNC_INV;
                return i + 1;
            }
        }
        bits.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts_packets(n: usize) -> Vec<[u8; TS_LEN]> {
        (0..n)
            .map(|i| {
                let mut p = [0u8; TS_LEN];
                p[0] = SYNC;
                p[1] = 0x01;
                p[2] = 0x00;
                p[3] = 0x10 | (i as u8 & 0x0F);
                for (k, b) in p.iter_mut().enumerate().skip(4) {
                    *b = (i * 13 + k * 7) as u8;
                }
                p
            })
            .collect()
    }

    fn to_bits(bytes: &[u8]) -> Vec<u8> {
        bytes
            .iter()
            .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1))
            .collect()
    }

    #[test]
    fn chain_round_trips_through_bits_at_any_offset() {
        let pkts = ts_packets(60);
        let mut enc = OuterEncoder::new();
        let mut stream = Vec::new();
        for p in &pkts {
            let mut o = [0u8; RS_LEN];
            enc.encode(p, &mut o);
            stream.extend_from_slice(&o);
        }
        for (skip, invert) in [(0, false), (5, false), (1000, true), (3333, false)] {
            let mut bits = vec![1, 0, 1];
            bits.extend(to_bits(&stream));
            let bits: Vec<u8> = bits[skip..]
                .iter()
                .map(|&b| if invert { b ^ 1 } else { b })
                .collect();
            let mut sync = TsSync::new();
            let mut dec = OuterDecoder::new();
            let mut out = Vec::new();
            sync.push(&bits, |pkt, _| {
                let mut ts = [0u8; TS_LEN];
                if dec.decode(pkt, &mut ts) {
                    out.push(ts);
                }
            });
            assert!(sync.locked());
            assert_eq!(sync.inverted(), invert);
            assert!(out.len() > 30, "{}", out.len());
            // Find where the output starts in the input.
            let at = pkts
                .iter()
                .position(|p| *p == out[0])
                .expect("first packet");
            assert_eq!(&pkts[at..at + out.len()], &out[..], "skip {skip}");
            assert_eq!(dec.failed, 0);
        }
    }

    #[test]
    fn polarity_flip_after_lock_is_followed() {
        // The bit stream inverts halfway (a 180° carrier slip): packets after
        // the flip still decode once the deinterleaver has flushed.
        let pkts = ts_packets(80);
        let mut enc = OuterEncoder::new();
        let mut stream = Vec::new();
        for p in &pkts {
            let mut o = [0u8; RS_LEN];
            enc.encode(p, &mut o);
            stream.extend_from_slice(&o);
        }
        let mut bits = to_bits(&stream);
        let half = 40 * PACKET_BITS + 77;
        for b in &mut bits[half..] {
            *b ^= 1;
        }
        let mut sync = TsSync::new();
        let mut dec = OuterDecoder::new();
        let mut out = Vec::new();
        sync.push(&bits, |pkt, _| {
            let mut ts = [0u8; TS_LEN];
            if dec.decode(pkt, &mut ts) {
                out.push(ts);
            }
        });
        assert_eq!(sync.flips, 1);
        assert_eq!(sync.losses, 0);
        let tail = &out[out.len() - 20..];
        let at = pkts
            .iter()
            .position(|p| *p == tail[0])
            .expect("tail decodes");
        assert_eq!(&pkts[at..at + 20], tail);
    }

    #[test]
    fn rs_failures_set_the_error_indicator() {
        let pkts = ts_packets(30);
        let mut enc = OuterEncoder::new();
        let mut dec = OuterDecoder::new();
        let mut outs = Vec::new();
        for (i, p) in pkts.iter().enumerate() {
            let mut o = [0u8; RS_LEN];
            enc.encode(p, &mut o);
            if i == 20 {
                // A burst of 120 bytes: 10 per codeword after deinterleaving.
                for b in &mut o[40..160] {
                    *b ^= 0x5A;
                }
            }
            let mut ts = [0u8; TS_LEN];
            if dec.decode(&o, &mut ts) {
                outs.push(ts);
            }
        }
        let bad: Vec<usize> = (0..outs.len()).filter(|&i| outs[i] != pkts[i]).collect();
        assert!(!bad.is_empty());
        assert_eq!(dec.failed as usize, bad.len());
        for &i in &bad {
            assert_eq!(outs[i][0], SYNC);
            assert_eq!(outs[i][1] & 0x80, 0x80, "packet {i}");
        }
    }
}
