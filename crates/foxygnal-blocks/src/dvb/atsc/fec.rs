//! The ATSC 1.0 (A/53 Part 2) data path, as plain structs with no block
//! code: [`Encoder`] turns TS packets into 8-level symbol indices, segment
//! and field syncs included; [`TrellisDecoder`] and [`OuterDecoder`] turn
//! equalized symbols back into TS packets.
//!
//! Transmit order:
//!
//! 1. The MPEG sync byte is dropped and the 187 bytes are **randomized**:
//!    XORed with the output of a 16-bit LFSR, G(x) = x¹⁶ + x¹³ + x¹² + x¹¹ +
//!    x⁷ + x⁶ + x³ + x + 1, preloaded with F180h at the start of every field.
//! 2. **RS(207, 187)**, t = 10 ([`super::super::rs::ATSC`]).
//! 3. The **byte interleaver**: 52 branches, M = 4 (branch j delays j·4
//!    visits), its commutator at branch 0 on each field's first byte (312
//!    segments of 207 bytes are a whole number of turns, so this holds by
//!    itself).
//! 4. The **trellis coder**: each byte becomes 4 dibits (x2 x1, MSB first)
//!    for one of 12 identical encoders. x2 goes through the precoder (z2 = x2
//!    ⊕ the encoder's previous z2), x1 is z1 and drives a 4-state rate-1/2
//!    code whose output z0 is the first of its two delay cells (b0' = x1 ⊕
//!    b1, b1' = b0). Symbol index z2 z1 z0 → level 2·index − 7. Which encoder
//!    takes which byte and sends where follows a pattern that repeats every 12
//!    segments ([`TrellisMap`]).
//! 5. Every segment: 4 sync symbols (+5 −5 −5 +5) and 828 data symbols; every
//!    313th segment is a **field sync**: PN511, three PN63 (the middle one
//!    inverted in the second field of a frame), the 24-symbol VSB mode, 92
//!    reserved symbols and a copy of the previous segment's last 12.

use std::sync::OnceLock;

use super::super::interleave::ConvInterleaver;
use super::super::rs::ATSC as RS;

/// Symbols per segment.
pub const SEGMENT: usize = 832;
/// Data symbols per segment.
pub const SEG_DATA: usize = 828;
/// Data segments per field.
pub const FIELD_SEGMENTS: usize = 312;
/// RS codeword (one per data segment).
pub const RS_N: usize = 207;
/// Payload bytes per packet (the MPEG sync byte isn't sent).
pub const PAYLOAD: usize = 187;
/// Trellis encoders (and decoders).
pub const CODERS: usize = 12;
/// Segments in one round of the trellis interleaving pattern.
pub const BLOCK_SEGMENTS: usize = 12;
/// Data bytes in such a round.
pub const BLOCK_BYTES: usize = BLOCK_SEGMENTS * RS_N;
/// Data symbols in such a round.
pub const BLOCK_SYMBOLS: usize = BLOCK_SEGMENTS * SEG_DATA;
/// Byte interleaver branches and unit.
pub const IL_BRANCHES: usize = 52;
pub const IL_M: usize = 4;
/// End-to-end delay of the byte interleaver pair, in bytes.
pub const IL_DELAY: usize = IL_BRANCHES * (IL_BRANCHES - 1) * IL_M;

/// Segment sync symbols as indices (+5 −5 −5 +5).
pub const SEG_SYNC: [u8; 4] = [6, 1, 1, 6];
/// Segment sync as levels.
pub const SEG_SYNC_LEVELS: [f32; 4] = [5.0, -5.0, -5.0, 5.0];

/// The symbols of the field sync segment that are known whatever the
/// station puts in the reserved part: sync, PN511, 3 × PN63, VSB mode.
pub const FIELD_SYNC_KNOWN: usize = 4 + 511 + 3 * 63 + 24;
/// Where the PN511 and the three PN63 start in the field sync segment.
pub const PN511_AT: usize = 4;
pub const PN63_AT: [usize; 3] = [515, 578, 641];

/// The VSB mode bits of 8-VSB (A/53 Part 2 §5.3.2.3.2).
const VSB_MODE: [u8; 24] = [
    0, 0, 0, 0, 1, 0, 1, 0, 0, 1, 0, 1, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 1, 0,
];

/// Level of a symbol index.
#[inline]
pub fn level(index: u8) -> f32 {
    (2 * index as i32 - 7) as f32
}

/// A binary sequence from the linear recurrence s[i] = ⊕ s[i − t] over
/// `taps`, starting from `init`.
fn recurrence<const N: usize>(init: &[u8], taps: &[usize]) -> [u8; N] {
    let mut s = [0u8; N];
    s[..init.len()].copy_from_slice(init);
    for i in init.len()..N {
        s[i] = taps.iter().fold(0, |a, &t| a ^ s[i - t]);
    }
    s
}

/// PN511: x⁹ + x⁷ + x⁶ + x⁴ + x³ + x + 1, preloaded 010000000.
pub fn pn511() -> &'static [u8; 511] {
    static T: OnceLock<[u8; 511]> = OnceLock::new();
    T.get_or_init(|| recurrence(&[0, 0, 0, 0, 0, 0, 0, 1, 0], &[2, 3, 5, 6, 8, 9]))
}

/// PN63: x⁶ + x + 1, preloaded 100111.
pub fn pn63() -> &'static [u8; 63] {
    static T: OnceLock<[u8; 63]> = OnceLock::new();
    T.get_or_init(|| recurrence(&[1, 1, 1, 0, 0, 1], &[5, 6]))
}

/// The field sync segment as symbol indices. `second` inverts the middle
/// PN63; `last12` are the previous segment's last 12 symbols. The 92
/// reserved symbols carry the PN63 again, as GNU Radio's transmitter does.
pub fn field_sync(second: bool, last12: &[u8; 12]) -> [u8; SEGMENT] {
    let bin = |b: u8| if b != 0 { 6 } else { 1 };
    let mut s = [0u8; SEGMENT];
    s[..4].copy_from_slice(&SEG_SYNC);
    let mut i = 4;
    for &b in pn511() {
        s[i] = bin(b);
        i += 1;
    }
    for k in 0..3 {
        let inv = u8::from(k == 1 && second);
        for &b in pn63() {
            s[i] = bin(b ^ inv);
            i += 1;
        }
    }
    for &b in &VSB_MODE {
        s[i] = bin(b);
        i += 1;
    }
    for j in 0..92 {
        s[i] = bin(pn63()[j % 63]);
        i += 1;
    }
    s[i..].copy_from_slice(last12);
    s
}

/// The levels of the field sync's known part (the reference the equalizer
/// trains on); the middle PN63 as in the first field.
pub fn field_sync_known() -> &'static [f32; FIELD_SYNC_KNOWN] {
    static T: OnceLock<[f32; FIELD_SYNC_KNOWN]> = OnceLock::new();
    T.get_or_init(|| {
        let s = field_sync(false, &[0; 12]);
        std::array::from_fn(|i| level(s[i]))
    })
}

/// The randomizer's bytes for a whole field: packet k of the field is
/// XORed with `[k·187 .. (k + 1)·187]`.
pub fn randomizer() -> &'static [u8] {
    static T: OnceLock<Vec<u8>> = OnceLock::new();
    T.get_or_init(|| {
        // The LFSR in Galois form, bit-reversed: bit 15 − i holds X^i's
        // stage, so F180h loads as 018Fh and the taps as A638h.
        let mut st: u16 = 0x018F;
        let mut out = Vec::with_capacity(FIELD_SEGMENTS * PAYLOAD);
        // Output bits D0…D7 come from stages X⁰, X², X³, X⁶, X¹⁰, X¹¹, X¹², X¹³.
        const OUT: [u16; 8] = [
            0x8000, 0x2000, 0x1000, 0x0200, 0x0020, 0x0010, 0x0008, 0x0004,
        ];
        for _ in 0..FIELD_SEGMENTS * PAYLOAD {
            let mut b = 0u8;
            for (bit, &m) in OUT.iter().enumerate() {
                if st & m != 0 {
                    b |= 1 << bit;
                }
            }
            out.push(b);
            st = if st & 1 != 0 {
                ((st ^ 0xA638) >> 1) | 0x8000
            } else {
                st >> 1
            };
        }
        out
    })
}

/// The trellis interleaving pattern of one round of 12 data segments: for
/// each data symbol (segment-major), its encoder and the byte and bit pair
/// it carries; for each encoder, its symbols in order.
pub struct TrellisMap {
    pub encoder: [u8; BLOCK_SYMBOLS],
    pub byte: [u16; BLOCK_SYMBOLS],
    pub shift: [u8; BLOCK_SYMBOLS],
    /// `order[e][k]`: the symbol (index into the arrays above) encoder `e`
    /// codes k-th.
    pub order: [[u16; BLOCK_SYMBOLS / CODERS]; CODERS],
}

/// The pattern (A/53 Part 2 §6.4.1.4 and GNU Radio's encoder): bytes go to
/// the encoders 12 at a time, each encoder sends its byte's four dibits in
/// four successive rounds of the 12 encoders, and every segment start moves
/// the encoder that goes first on by 4. Exactly when that move happens
/// relative to loading the next 12 bytes is what makes it the standard's
/// pattern.
pub fn trellis_map() -> &'static TrellisMap {
    static T: OnceLock<Box<TrellisMap>> = OnceLock::new();
    T.get_or_init(|| {
        let mut m = Box::new(TrellisMap {
            encoder: [0; BLOCK_SYMBOLS],
            byte: [0; BLOCK_SYMBOLS],
            shift: [0; BLOCK_SYMBOLS],
            order: [[0; BLOCK_SYMBOLS / CODERS]; CODERS],
        });
        let mut encoder = CODERS - 4;
        let mut skip_bump = false;
        let mut out = 0;
        let mut next_seg = 0;
        let mut loaded = [0u16; CODERS];
        let mut count = [0usize; CODERS];
        for chunk in (0..BLOCK_BYTES).step_by(CODERS) {
            if out >= next_seg {
                encoder = (encoder + 4) % CODERS;
                skip_bump = true;
            }
            for i in 0..CODERS {
                loaded[encoder] = (chunk + i) as u16;
                encoder = (encoder + 1) % CODERS;
            }
            for shift in [6u8, 4, 2, 0] {
                if out >= next_seg {
                    next_seg = out + SEG_DATA;
                    if !skip_bump {
                        encoder = (encoder + 4) % CODERS;
                    }
                    skip_bump = false;
                }
                for _ in 0..CODERS {
                    m.encoder[out] = encoder as u8;
                    m.byte[out] = loaded[encoder];
                    m.shift[out] = shift;
                    m.order[encoder][count[encoder]] = out as u16;
                    count[encoder] += 1;
                    out += 1;
                    encoder = (encoder + 1) % CODERS;
                }
            }
        }
        assert_eq!(out, BLOCK_SYMBOLS);
        assert_eq!(encoder, CODERS - 4);
        m
    })
}

/// One trellis encoder with its precoder.
#[derive(Clone, Copy, Default)]
struct TrellisEncoder {
    /// Precoder memory (the last z2).
    z2: u8,
    /// The two delay cells of the rate-1/2 code.
    b0: u8,
    b1: u8,
}

impl TrellisEncoder {
    /// Dibit x2 x1 → symbol index z2 z1 z0.
    #[inline]
    fn encode(&mut self, dibit: u8) -> u8 {
        let x2 = dibit >> 1;
        let x1 = dibit & 1;
        let z2 = x2 ^ self.z2;
        let z0 = self.b0;
        self.z2 = z2;
        let b0 = x1 ^ self.b1;
        self.b1 = self.b0;
        self.b0 = b0;
        (z2 << 2) | (x1 << 1) | z0
    }
}

/// TS packets → symbol indices, segment by segment.
pub struct Encoder {
    il: ConvInterleaver,
    /// Data segment of the next packet within its field (0..312).
    seg: usize,
    second_field: bool,
    block: [u8; BLOCK_BYTES],
    in_block: usize,
    enc: [TrellisEncoder; CODERS],
    last12: [u8; 12],
    cw: [u8; RS_N],
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            il: ConvInterleaver::with(IL_BRANCHES, IL_M, false),
            seg: 0,
            second_field: false,
            block: [0; BLOCK_BYTES],
            in_block: 0,
            enc: [TrellisEncoder::default(); CODERS],
            last12: [0; 12],
            cw: [0; RS_N],
        }
    }

    /// Add one TS packet (`ts[0]`, the sync byte, is ignored). Every 12th
    /// call appends 12 segments of symbols to `out` (13 when a field starts:
    /// its field sync first) and returns true.
    pub fn push(&mut self, ts: &[u8; 188], out: &mut Vec<u8>) -> bool {
        let rand = &randomizer()[self.seg * PAYLOAD..(self.seg + 1) * PAYLOAD];
        for ((c, &b), &r) in self.cw.iter_mut().zip(&ts[1..]).zip(rand) {
            *c = b ^ r;
        }
        RS.encode(&mut self.cw);
        let at = self.in_block * RS_N;
        for (o, &b) in self.block[at..at + RS_N].iter_mut().zip(&self.cw) {
            *o = self.il.push(b);
        }
        self.seg += 1;
        self.in_block += 1;
        if self.in_block < BLOCK_SEGMENTS {
            return false;
        }
        self.in_block = 0;
        // This block's first segment.
        let first = self.seg - BLOCK_SEGMENTS;
        if first == 0 {
            out.extend_from_slice(&field_sync(self.second_field, &self.last12));
        }
        let map = trellis_map();
        let start = out.len();
        for s in 0..BLOCK_SEGMENTS {
            out.extend_from_slice(&SEG_SYNC);
            for p in s * SEG_DATA..(s + 1) * SEG_DATA {
                let dibit = (self.block[map.byte[p] as usize] >> map.shift[p]) & 3;
                out.push(self.enc[map.encoder[p] as usize].encode(dibit));
            }
        }
        if self.seg == FIELD_SEGMENTS {
            let end = start + BLOCK_SEGMENTS * SEGMENT;
            self.last12.copy_from_slice(&out[end - 12..end]);
            self.seg = 0;
            self.second_field = !self.second_field;
        }
        true
    }
}

// ---- Receiver -------------------------------------------------------------------

/// Steps a Viterbi decoder looks ahead before deciding, and decides at once.
const TB_DEPTH: usize = 40;
const TB_OUT: usize = 24;
const TB_RING: usize = TB_DEPTH + TB_OUT;

/// The smaller of two metrics. (`f32::min` also handles NaN, which costs
/// extra instructions on x86; `step` keeps NaN out of the metrics.)
#[inline(always)]
fn min(a: f32, b: f32) -> f32 {
    if b < a { b } else { a }
}

/// One 4-state Viterbi decoder for one trellis encoder's symbols. Each
/// branch has two parallel transitions (z2 = 0 or 1, 8 levels apart): the
/// branch metric is the nearer of the two, and which one is remembered.
#[derive(Clone, Copy)]
struct Viterbi {
    pm: [f32; 4],
    /// Per step: bit s = the survivor into state s came from b1 = 1; bit
    /// 4 + c = z2 was 1 on subset c (= z1 z0).
    dec: [u8; TB_RING],
    steps: usize,
    z2: u8,
}

impl Viterbi {
    fn new() -> Self {
        Self {
            pm: [0.0; 4],
            dec: [0; TB_RING],
            steps: 0,
            z2: 0,
        }
    }

    /// One soft symbol (in level units). When TB_OUT steps can be decided,
    /// writes their dibits (x2 x1) to `out` and returns true.
    #[inline]
    fn step(&mut self, r: f32, out: &mut [u8; TB_OUT]) -> bool {
        // An equalizer gone non-finite (it resets at the next segment) must
        // not leave NaN in the path metrics for good.
        let r = if r.is_finite() { r } else { 0.0 };
        let mut bm = [0f32; 4];
        let mut z2 = 0u8;
        for (c, m) in bm.iter_mut().enumerate() {
            let lo = r - (2 * c as i32 - 7) as f32;
            let hi = lo - 8.0;
            let (dl, dh) = (lo * lo, hi * hi);
            z2 |= ((dh < dl) as u8) << c;
            *m = min(dl, dh);
        }
        // State s = b1 b0. Into s = (c1, c0) from (b1, c1) with x1 = c0 ⊕ b1;
        // the branch's subset is z1 z0 = x1 c1.
        let pm = self.pm;
        let mut new = [0f32; 4];
        let mut d = z2 << 4;
        for (s, n) in new.iter_mut().enumerate() {
            let (c1, c0) = (s >> 1, s & 1);
            let m0 = pm[c1] + bm[(c0 << 1) | c1];
            let m1 = pm[2 + c1] + bm[((c0 ^ 1) << 1) | c1];
            d |= ((m1 < m0) as u8) << s;
            *n = min(m0, m1);
        }
        let low = min(min(new[0], new[1]), min(new[2], new[3]));
        for (p, n) in self.pm.iter_mut().zip(new) {
            *p = n - low;
        }
        self.dec[self.steps] = d;
        self.steps += 1;
        if self.steps < TB_RING {
            return false;
        }
        // Trace back from the best state.
        let mut s = (0..4)
            .min_by(|&a, &b| self.pm[a].total_cmp(&self.pm[b]))
            .unwrap_or(0);
        for t in (TB_OUT..TB_RING).rev() {
            let b1 = (self.dec[t] >> s) & 1;
            s = ((b1 as usize) << 1) | (s >> 1);
        }
        let mut x = [(0u8, 0u8); TB_OUT];
        for t in (0..TB_OUT).rev() {
            let dt = self.dec[t];
            let b1 = (dt >> s) & 1;
            let (c1, c0) = ((s >> 1) as u8, (s & 1) as u8);
            let x1 = c0 ^ b1;
            let z2 = (dt >> (4 + ((x1 << 1) | c1))) & 1;
            x[t] = (x1, z2);
            s = ((b1 as usize) << 1) | (s >> 1);
        }
        for (o, (x1, z2)) in out.iter_mut().zip(x) {
            *o = ((z2 ^ self.z2) << 1) | x1;
            self.z2 = z2;
        }
        self.dec.copy_within(TB_OUT.., 0);
        self.steps = TB_DEPTH;
        true
    }
}

/// The 12 Viterbi decoders and the trellis deinterleaving: data symbols in
/// (from a field's first data segment on, in order), rounds of 12 segments'
/// bytes (still byte-interleaved) out.
pub struct TrellisDecoder {
    map: &'static TrellisMap,
    vit: [Viterbi; CODERS],
    /// Data symbol within the round being received.
    sym: usize,
    /// Dibits each encoder has output.
    emitted: [usize; CODERS],
    bufs: [[u8; BLOCK_BYTES]; 2],
    filled: [usize; 2],
    done: [u8; BLOCK_BYTES],
    dibits: [u8; TB_OUT],
}

impl Default for TrellisDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl TrellisDecoder {
    pub fn new() -> Self {
        Self {
            map: trellis_map(),
            vit: [Viterbi::new(); CODERS],
            sym: 0,
            emitted: [0; CODERS],
            bufs: [[0; BLOCK_BYTES]; 2],
            filled: [0; 2],
            done: [0; BLOCK_BYTES],
            dibits: [0; TB_OUT],
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// One equalized data symbol, in level units. Returns true when a round
    /// is complete: [`Self::round`] holds its bytes.
    #[inline]
    pub fn push(&mut self, r: f32) -> bool {
        let map = self.map;
        let e = map.encoder[self.sym] as usize;
        self.sym += 1;
        if self.sym == BLOCK_SYMBOLS {
            self.sym = 0;
        }
        if !self.vit[e].step(r, &mut self.dibits) {
            return false;
        }
        let mut complete = false;
        let per = BLOCK_SYMBOLS / CODERS;
        for k in 0..TB_OUT {
            let n = self.emitted[e];
            self.emitted[e] += 1;
            let round = (n / per) & 1;
            let p = map.order[e][n % per] as usize;
            let buf = &mut self.bufs[round];
            buf[map.byte[p] as usize] |= self.dibits[k] << map.shift[p];
            self.filled[round] += 1;
            if self.filled[round] == BLOCK_SYMBOLS {
                self.done = *buf;
                buf.fill(0);
                self.filled[round] = 0;
                complete = true;
            }
        }
        complete
    }

    /// The bytes of the round last completed.
    pub fn round(&self) -> &[u8; BLOCK_BYTES] {
        &self.done
    }
}

/// Byte deinterleaving, RS decoding and derandomizing: rounds of bytes from
/// the [`TrellisDecoder`] (starting at a field's first) in, TS packets out.
pub struct OuterDecoder {
    dil: ConvInterleaver,
    /// Bytes out of the deinterleaver so far.
    count: usize,
    cw: [u8; RS_N],
    fill: usize,
    /// Packets out, bytes corrected, packets RS couldn't correct.
    pub packets: u64,
    pub corrected: u64,
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
            dil: ConvInterleaver::with(IL_BRANCHES, IL_M, true),
            count: 0,
            cw: [0; RS_N],
            fill: 0,
            packets: 0,
            corrected: 0,
            failed: 0,
        }
    }

    /// Start again (a new field grid); the counters stay.
    pub fn reset(&mut self) {
        self.dil.reset();
        self.count = 0;
        self.fill = 0;
    }

    /// Bytes of one round; calls `out` with each packet (188 bytes, the
    /// transport_error_indicator set if RS failed).
    pub fn push(&mut self, bytes: &[u8], mut out: impl FnMut(&[u8; 188])) {
        for &b in bytes {
            let o = self.dil.push(b);
            let c = self.count;
            self.count += 1;
            // Packet k of the field we started on comes out IL_DELAY bytes
            // after it went in; anything before is older.
            if c < IL_DELAY {
                continue;
            }
            self.cw[self.fill] = o;
            self.fill += 1;
            if self.fill < RS_N {
                continue;
            }
            self.fill = 0;
            let k = (c + 1 - IL_DELAY) / RS_N - 1;
            let seg = k % FIELD_SEGMENTS;
            if self.count >= IL_DELAY + FIELD_SEGMENTS * RS_N * 4 {
                // Keep the counter small; whole fields only.
                self.count -= FIELD_SEGMENTS * RS_N;
            }
            let ok = match RS.decode(&mut self.cw) {
                Ok(n) => {
                    self.corrected += n as u64;
                    true
                }
                Err(_) => {
                    self.failed += 1;
                    false
                }
            };
            let rand = &randomizer()[seg * PAYLOAD..(seg + 1) * PAYLOAD];
            let mut pkt = [0u8; 188];
            pkt[0] = 0x47;
            for ((p, &c), &r) in pkt[1..].iter_mut().zip(&self.cw[..PAYLOAD]).zip(rand) {
                *p = c ^ r;
            }
            if !ok {
                pkt[1] |= 0x80;
            }
            self.packets += 1;
            out(&pkt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pn_sequences_match_the_standard() {
        // A/53 Part 2 table 5.x, as GNU Radio tabulates it.
        let pn = pn511();
        let head: String = pn[..24].iter().map(|b| char::from(b'0' + b)).collect();
        assert_eq!(head, "000000010111111111001010");
        assert_eq!(pn.iter().filter(|&&b| b == 1).count(), 256);
        let p63 = pn63();
        let head: String = p63[..12].iter().map(|b| char::from(b'0' + b)).collect();
        assert_eq!(head, "111001001011");
        assert_eq!(p63.iter().filter(|&&b| b == 1).count(), 32);
    }

    #[test]
    fn randomizer_starts_as_published() {
        // The first bytes of the randomizing sequence (A/53 Part 2 §6.3.1).
        let r = randomizer();
        assert_eq!(&r[..4], &[0xC0, 0x6D, 0x3F, 0x99][..]);
    }

    #[test]
    fn trellis_map_is_a_permutation() {
        let m = trellis_map();
        let mut seen = vec![0u8; BLOCK_BYTES];
        for p in 0..BLOCK_SYMBOLS {
            seen[m.byte[p] as usize] |= 1 << (m.shift[p] / 2);
            // Within a segment, encoders take turns.
            if p % SEG_DATA != 0 {
                assert_eq!(
                    m.encoder[p] as usize,
                    (m.encoder[p - 1] as usize + 1) % CODERS
                );
            }
        }
        assert!(seen.iter().all(|&s| s == 0xF));
        // Segment s starts with encoder 4s mod 12.
        for s in 0..BLOCK_SEGMENTS {
            assert_eq!(m.encoder[s * SEG_DATA] as usize, (4 * s) % CODERS);
        }
    }

    #[test]
    fn viterbi_decodes_clean_and_noisy_symbols() {
        let mut seed = 0x1234_5678u64;
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let n = 4000;
        let dibits: Vec<u8> = (0..n).map(|_| (rng() & 3) as u8).collect();
        let mut enc = TrellisEncoder::default();
        let syms: Vec<f32> = dibits.iter().map(|&d| level(enc.encode(d))).collect();
        for noise in [0.0f32, 0.6] {
            let mut v = Viterbi::new();
            let mut out = [0u8; TB_OUT];
            let mut got = Vec::new();
            for &s in &syms {
                let u = (rng() >> 11) as f32 / (1u64 << 53) as f32 - 0.5;
                if v.step(s + noise * u * 3.4, &mut out) {
                    got.extend_from_slice(&out);
                }
            }
            // The first x2 needs the previous z2 (unknown: 0 here, as sent).
            let errors = got.iter().zip(&dibits).filter(|(a, b)| a != b).count();
            assert_eq!(errors, 0, "noise {noise}");
            assert!(got.len() > n - TB_RING - TB_OUT);
        }
    }

    #[test]
    fn encoder_and_decoders_round_trip() {
        let mut enc = Encoder::new();
        let mut syms = Vec::new();
        let n = 3 * FIELD_SEGMENTS;
        let packets: Vec<[u8; 188]> = (0..n)
            .map(|i| {
                let mut p = [0u8; 188];
                p[0] = 0x47;
                for (k, b) in p.iter_mut().enumerate().skip(1) {
                    *b = (i * 7 + k * 13) as u8;
                }
                p
            })
            .collect();
        for p in &packets {
            enc.push(p, &mut syms);
        }
        assert_eq!(syms.len(), 3 * (FIELD_SEGMENTS + 1) * SEGMENT);
        // Field syncs: PN63 middle normal, inverted, normal.
        for f in 0..3 {
            let fs = &syms[f * 313 * SEGMENT..];
            assert_eq!(fs[PN511_AT], 1);
            let mid = (fs[PN63_AT[1]] == fs[PN63_AT[0]]) as usize;
            assert_eq!(mid, usize::from(f % 2 == 0), "field {f}");
        }
        let mut tdec = TrellisDecoder::new();
        let mut outer = OuterDecoder::new();
        let mut got = Vec::new();
        for f in 0..3 {
            for s in 1..313 {
                let seg = &syms[(f * 313 + s) * SEGMENT..(f * 313 + s + 1) * SEGMENT];
                assert_eq!(&seg[..4], &SEG_SYNC);
                for &x in &seg[4..] {
                    if tdec.push(level(x)) {
                        outer.push(tdec.round(), |p| got.push(*p));
                    }
                }
            }
        }
        assert!(got.len() > n - 52 - 20, "{}", got.len());
        for (k, p) in got.iter().enumerate() {
            assert_eq!(p[..], packets[k][..], "packet {k}");
        }
        assert_eq!(outer.failed, 0);
    }
}
