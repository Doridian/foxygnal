//! Rice coding as in CCSDS 121.0-B (lossless data compression), the way szip
//! and libaec do it: GOES packs each line of an image this way.
//!
//! Samples go through an optional unit-delay predictor ("preprocessing"),
//! then blocks of `J` mapped residuals are coded with whichever option is
//! shortest: a run of zero blocks, the second extension (pairs), a
//! split-sample code with `k` low bits sent as they are, or no coding at all.
//! Every `rsi` blocks (a reference sample interval) the predictor restarts
//! with a sample sent in full.
//!
//! [`decode`] and [`encode`] work on samples; [`Szip`] adds the szip
//! conventions (one RSI per scan line, lines padded to whole blocks, samples
//! as 1, 2 or 4 bytes). Checked against libaec (`SZ_BufftoBuff*`) with the
//! vectors in `tests/fixtures/xrit/`.

/// Coding parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Bits per sample, 1…32.
    pub bits: u32,
    /// Samples per block (8, 16, 32 or 64; others work but aren't standard).
    pub block: usize,
    /// Blocks per reference sample interval.
    pub rsi: usize,
    /// Unit-delay prediction (szip's "NN" option).
    pub preprocess: bool,
    /// The restricted set of options for 1–4 bit samples.
    pub restricted: bool,
    /// Each RSI starts on a byte boundary (libaec `AEC_PAD_RSI`; not szip).
    pub pad_rsi: bool,
}

/// Why decoding stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The input ended before all samples were decoded.
    Truncated,
    /// An impossible code (a second-extension value out of range).
    Invalid,
    /// Parameters out of range.
    Params,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::Truncated => "compressed data ends early",
            Error::Invalid => "invalid compressed data",
            Error::Params => "unsupported compression parameters",
        })
    }
}

impl Params {
    fn check(&self) -> Result<(), Error> {
        let ok = (1..=32).contains(&self.bits)
            && (2..=4096).contains(&self.block)
            && self.block.is_multiple_of(2)
            && (1..=4096).contains(&self.rsi);
        if ok { Ok(()) } else { Err(Error::Params) }
    }

    /// Bits of the option identifier.
    fn id_len(&self) -> u32 {
        match self.bits {
            17.. => 5,
            9..=16 => 4,
            3..=4 if self.restricted => 2,
            ..=2 if self.restricted => 1,
            _ => 3,
        }
    }

    fn xmax(&self) -> u32 {
        if self.bits == 32 {
            u32::MAX
        } else {
            (1 << self.bits) - 1
        }
    }
}

/// A run of this many zero blocks (as coded) means "to the end of the
/// segment" (64 blocks) or of the RSI, whichever comes first.
const ROS: u32 = 5;
/// The largest value a second-extension code may have (pairs summing to 12).
const SE_MAX: u32 = 90;

struct BitReader<'a> {
    data: &'a [u8],
    /// Bit position.
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// The next up to 57 bits, MSB first, left-aligned in a u64 (zeros past
    /// the end).
    fn peek64(&self) -> u64 {
        let byte = self.pos / 8;
        let mut w = [0u8; 8];
        let end = (byte + 8).min(self.data.len());
        if byte < end {
            w[..end - byte].copy_from_slice(&self.data[byte..end]);
        }
        u64::from_be_bytes(w) << (self.pos % 8)
    }

    fn read(&mut self, n: u32) -> Result<u32, Error> {
        if n == 0 {
            return Ok(0);
        }
        if self.left() < n as usize {
            return Err(Error::Truncated);
        }
        let v = (self.peek64() >> (64 - n)) as u32;
        self.pos += n as usize;
        Ok(v)
    }

    /// A fundamental sequence: the number of zeros before the next one.
    fn fs(&mut self) -> Result<u32, Error> {
        let mut count = 0u32;
        loop {
            let w = self.peek64();
            let avail = self.left().min(57) as u32;
            if avail == 0 {
                return Err(Error::Truncated);
            }
            let z = w.leading_zeros();
            if z < avail {
                self.pos += z as usize + 1;
                return Ok(count + z);
            }
            count += avail;
            self.pos += avail as usize;
        }
    }

    fn align(&mut self) {
        self.pos = self.pos.next_multiple_of(8);
    }
}

/// Undo the predictor: residual `d` after prediction `pred`.
#[inline]
fn unmap(d: u32, pred: u32, xmax: u32) -> u32 {
    let theta = pred.min(xmax - pred);
    if d <= 2 * theta {
        if d & 1 == 0 {
            pred + d / 2
        } else {
            pred - d.div_ceil(2)
        }
    } else if theta == pred {
        d
    } else {
        xmax - d
    }
}

/// Map sample `x` after prediction `pred` to a residual.
#[inline]
fn map(x: u32, pred: u32, xmax: u32) -> u32 {
    let theta = pred.min(xmax - pred);
    let (up, delta) = if x >= pred {
        (true, x - pred)
    } else {
        (false, pred - x)
    };
    if delta <= theta {
        if up { 2 * delta } else { 2 * delta - 1 }
    } else {
        theta + delta
    }
}

/// Where decoded samples go: applies the predictor.
struct Sink<'a> {
    out: &'a mut Vec<u32>,
    total: usize,
    preprocess: bool,
    xmax: u32,
    pred: u32,
}

impl Sink<'_> {
    fn full(&self) -> bool {
        self.out.len() >= self.total
    }

    /// The reference sample (sent as it is).
    fn reference(&mut self, x: u32) {
        if !self.full() {
            self.out.push(x);
            self.pred = x;
        }
    }

    fn residual(&mut self, d: u32) {
        if self.full() {
            return;
        }
        let x = if self.preprocess {
            let x = unmap(d.min(self.xmax), self.pred, self.xmax);
            self.pred = x;
            x
        } else {
            d
        };
        self.out.push(x);
    }
}

/// Decode `samples` samples from `input`, appending them to `out`. On error
/// the samples decoded so far stay in `out`.
pub fn decode(p: &Params, input: &[u8], samples: usize, out: &mut Vec<u32>) -> Result<(), Error> {
    p.check()?;
    out.reserve(samples);
    let id_len = p.id_len();
    let uncomp = (1u32 << id_len) - 1;
    let kmax = uncomp.saturating_sub(2);
    let mut br = BitReader::new(input);
    let mut sink = Sink {
        total: out.len() + samples,
        out,
        preprocess: p.preprocess,
        xmax: p.xmax(),
        pred: 0,
    };
    let block = p.block;
    let mut fs = vec![0u32; block];
    while !sink.full() {
        let mut b = 0;
        while b < p.rsi && !sink.full() {
            let refs = usize::from(p.preprocess && b == 0);
            let id = br.read(id_len)?;
            if id == 0 {
                let second = br.read(1)? == 1;
                if refs == 1 {
                    sink.reference(br.read(p.bits)?);
                }
                if second {
                    let mut i = refs;
                    while i < block {
                        let m = br.fs()?;
                        if m > SE_MAX {
                            return Err(Error::Invalid);
                        }
                        // m = s(s+1)/2 + second, with s = first + second.
                        let mut s = 0;
                        while (s + 1) * (s + 2) / 2 <= m {
                            s += 1;
                        }
                        let d1 = m - s * (s + 1) / 2;
                        if i & 1 == 0 {
                            sink.residual(s - d1);
                            i += 1;
                        }
                        sink.residual(d1);
                        i += 1;
                    }
                    b += 1;
                } else {
                    let mut run = br.fs()? + 1;
                    if run == ROS {
                        run = ((p.rsi - b) as u32).min(64 - (b % 64) as u32);
                    } else if run > ROS {
                        run -= 1;
                    }
                    for _ in 0..run as usize * block - refs {
                        sink.residual(0);
                    }
                    b += run as usize;
                }
            } else if id == uncomp {
                if refs == 1 {
                    sink.reference(br.read(p.bits)?);
                }
                for _ in refs..block {
                    sink.residual(br.read(p.bits)?);
                }
                b += 1;
            } else {
                let k = id - 1;
                debug_assert!(k <= kmax);
                if refs == 1 {
                    sink.reference(br.read(p.bits)?);
                }
                for f in &mut fs[refs..] {
                    *f = br.fs()?;
                }
                for &f in &fs[refs..] {
                    let low = br.read(k)?;
                    sink.residual(f.checked_shl(k).unwrap_or(0) | low);
                }
                b += 1;
            }
        }
        if p.pad_rsi {
            br.align();
        }
    }
    Ok(())
}

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter<'_> {
    fn put(&mut self, v: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        let v = u64::from(v) & ((1u64 << bits) - 1);
        self.acc = (self.acc << bits) | v;
        self.n += bits;
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n) as u8);
        }
    }

    fn fs(&mut self, mut zeros: u32) {
        while zeros >= 32 {
            self.put(0, 32);
            zeros -= 32;
        }
        self.put(1, zeros + 1);
    }

    fn align(&mut self) {
        if self.n > 0 {
            self.put(0, 8 - self.n);
        }
    }
}

/// Encode `samples` (each below 2^bits), appending to `out`. Picks the
/// shortest option for every block; never codes runs of zero blocks longer
/// than one (always valid, slightly longer than libaec on flat data).
pub fn encode(p: &Params, samples: &[u32], out: &mut Vec<u8>) -> Result<(), Error> {
    p.check()?;
    let id_len = p.id_len();
    let uncomp = (1u32 << id_len) - 1;
    let kmax = uncomp.saturating_sub(2);
    let xmax = p.xmax();
    let mut w = BitWriter { out, acc: 0, n: 0 };
    let rsi_len = p.rsi * p.block;
    for interval in samples.chunks(rsi_len) {
        // Residuals of every block of the interval (the first sample of the
        // first block is the reference when predicting).
        let mut pred = 0;
        let blocks: Vec<Vec<u32>> = interval
            .chunks(p.block)
            .enumerate()
            .map(|(bi, chunk)| {
                (0..p.block)
                    .map(|i| {
                        // A short last block is padded with its last sample.
                        let x = chunk[i.min(chunk.len() - 1)].min(xmax);
                        if !p.preprocess {
                            x
                        } else if bi == 0 && i == 0 {
                            pred = x;
                            x
                        } else {
                            let m = map(x, pred, xmax);
                            pred = x;
                            m
                        }
                    })
                    .collect()
            })
            .collect();
        let zero = |bi: usize| {
            blocks[bi][usize::from(p.preprocess && bi == 0)..]
                .iter()
                .all(|&x| x == 0)
        };
        let mut bi = 0;
        while bi < blocks.len() {
            let d = &blocks[bi];
            let refs = usize::from(p.preprocess && bi == 0);
            if zero(bi) {
                // A run of zero blocks, never past the end of a 64-block
                // segment.
                let mut end = bi + 1;
                while end < blocks.len() && end % 64 != 0 && zero(end) {
                    end += 1;
                }
                let run = (end - bi) as u32;
                let to_end = end % 64 == 0 || end == p.rsi;
                w.put(0, id_len + 1);
                if refs == 1 {
                    w.put(d[0], p.bits);
                }
                w.fs(if run >= ROS && to_end {
                    ROS - 1
                } else if run >= ROS {
                    run
                } else {
                    run - 1
                });
                bi = end;
                continue;
            }
            let res = &d[refs..];
            let ref_bits = if refs == 1 { u64::from(p.bits) } else { 0 };
            // Costs in bits of each option.
            let mut best = (
                u64::from(id_len) + u64::from(p.bits) * p.block as u64,
                uncomp,
            );
            for k in 0..=kmax {
                if k >= p.bits {
                    break;
                }
                let cost = u64::from(id_len)
                    + ref_bits
                    + res
                        .iter()
                        .map(|&x| u64::from(x >> k) + 1 + u64::from(k))
                        .sum::<u64>();
                if cost < best.0 {
                    best = (cost, k + 1);
                }
            }
            if let Some(cost) = se_cost(d, refs) {
                let cost = u64::from(id_len) + 1 + ref_bits + cost;
                if cost < best.0 {
                    best = (cost, u32::MAX);
                }
            }
            match best.1 {
                u32::MAX => {
                    w.put(0, id_len);
                    w.put(1, 1);
                    if refs == 1 {
                        w.put(d[0], p.bits);
                    }
                    let mut i = refs;
                    while i < p.block {
                        let (a, b) = if i & 1 == 0 {
                            (d[i], d[i + 1])
                        } else {
                            (0, d[i])
                        };
                        let s = a + b;
                        w.fs(s * (s + 1) / 2 + b);
                        i += if i & 1 == 0 { 2 } else { 1 };
                    }
                }
                id if id == uncomp => {
                    w.put(id, id_len);
                    for &x in d {
                        w.put(x, p.bits);
                    }
                }
                id => {
                    let k = id - 1;
                    w.put(id, id_len);
                    if refs == 1 {
                        w.put(d[0], p.bits);
                    }
                    for &x in res {
                        w.fs(x >> k);
                    }
                    for &x in res {
                        w.put(x, k);
                    }
                }
            }
            bi += 1;
        }
        if p.pad_rsi {
            w.align();
        }
    }
    w.align();
    Ok(())
}

/// Bits of the second-extension codes for a block (`d`, whose first sample
/// is the reference if `refs` is 1), if every pair fits.
fn se_cost(d: &[u32], refs: usize) -> Option<u64> {
    let mut cost = 0u64;
    let mut i = refs;
    while i < d.len() {
        let (a, b) = if i & 1 == 0 {
            (d[i], d[i + 1])
        } else {
            (0, d[i])
        };
        i += if i & 1 == 0 { 2 } else { 1 };
        let s = a.checked_add(b)?;
        if s > 12 {
            return None;
        }
        let m = s * (s + 1) / 2 + b;
        if m > SE_MAX {
            return None;
        }
        cost += u64::from(m) + 1;
    }
    Some(cost)
}

/// szip's view of the same code, as GOES (and HDF) use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Szip {
    /// The szip options mask (`SZ_*_OPTION_MASK`): 16 MSB first, 32 NN
    /// (prediction), 8 LSB first, 128 raw; 1 (K13), 2 (chip) and 4 (EC) say
    /// nothing a decoder needs.
    pub options: u16,
    pub bits_per_pixel: u8,
    pub pixels_per_block: u16,
    pub pixels_per_scanline: usize,
}

pub const SZ_MSB: u16 = 16;
pub const SZ_NN: u16 = 32;
pub const SZ_RAW: u16 = 128;

impl Szip {
    fn params(&self) -> Result<Params, Error> {
        if self.pixels_per_block == 0 || self.pixels_per_scanline == 0 {
            return Err(Error::Params);
        }
        let block = usize::from(self.pixels_per_block);
        Ok(Params {
            bits: u32::from(self.bits_per_pixel),
            block,
            rsi: self.pixels_per_scanline.div_ceil(block),
            preprocess: self.options & SZ_NN != 0,
            restricted: false,
            pad_rsi: false,
        })
    }

    /// Bytes per sample in the uncompressed buffer.
    pub fn sample_bytes(&self) -> usize {
        match self.bits_per_pixel {
            ..=8 => 1,
            9..=16 => 2,
            _ => 4,
        }
    }

    /// Decompress into `out_len` bytes (`out_len / sample_bytes` samples,
    /// whole scan lines). On error, the lines decoded so far are returned
    /// with the error; the rest of `out` is zero.
    pub fn decompress(&self, input: &[u8], out_len: usize) -> (Vec<u8>, Result<(), Error>) {
        let mut out = vec![0u8; out_len];
        let p = match self.params() {
            Ok(p) => p,
            Err(e) => return (out, Err(e)),
        };
        let sb = self.sample_bytes();
        let pixels = out_len / sb;
        let ppl = self.pixels_per_scanline;
        let padded_line = p.rsi * p.block;
        let lines = pixels.div_ceil(ppl);
        let mut samples = Vec::with_capacity(lines * padded_line);
        let result = decode(&p, input, lines * padded_line, &mut samples);
        let msb = self.options & SZ_MSB != 0;
        for (i, &x) in samples.iter().enumerate() {
            let (line, col) = (i / padded_line, i % padded_line);
            if col >= ppl {
                continue;
            }
            let at = (line * ppl + col) * sb;
            if at + sb > out.len() {
                break;
            }
            let bytes = x.to_be_bytes();
            let dst = &mut out[at..at + sb];
            dst.copy_from_slice(&bytes[4 - sb..]);
            if !msb {
                dst.reverse();
            }
        }
        (out, result)
    }

    /// Compress whole scan lines (`input.len() / sample_bytes` samples).
    pub fn compress(&self, input: &[u8]) -> Result<Vec<u8>, Error> {
        let p = self.params()?;
        let sb = self.sample_bytes();
        let msb = self.options & SZ_MSB != 0;
        let ppl = self.pixels_per_scanline;
        let padded_line = p.rsi * p.block;
        let pixels = input.len() / sb;
        let mut samples = Vec::with_capacity(pixels.div_ceil(ppl) * padded_line);
        for line in input.chunks(ppl * sb) {
            let start = samples.len();
            for px in line.chunks_exact(sb) {
                let mut b = [0u8; 4];
                b[4 - sb..].copy_from_slice(px);
                if !msb {
                    b[4 - sb..].reverse();
                }
                samples.push(u32::from_be_bytes(b));
            }
            let last = samples.last().copied().unwrap_or(0);
            samples.resize(start + padded_line, last);
        }
        let mut out = Vec::new();
        encode(&p, &samples, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(bits: u32, block: usize, rsi: usize, preprocess: bool) -> Params {
        Params {
            bits,
            block,
            rsi,
            preprocess,
            restricted: false,
            pad_rsi: false,
        }
    }

    fn lcg(seed: &mut u64) -> u32 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (*seed >> 33) as u32
    }

    #[test]
    fn mapping_round_trips() {
        for xmax in [1u32, 255, 1023] {
            for pred in 0..=xmax.min(300) {
                for x in 0..=xmax.min(300) {
                    let d = map(x, pred, xmax);
                    assert!(d <= xmax);
                    assert_eq!(unmap(d, pred, xmax), x, "x={x} pred={pred} xmax={xmax}");
                }
            }
        }
    }

    #[test]
    fn round_trips() {
        let mut seed = 7;
        for bits in [1u32, 4, 8, 10, 12, 16, 20, 32] {
            for (block, rsi) in [(8, 4), (16, 128), (32, 3), (64, 2)] {
                for preprocess in [false, true] {
                    for shape in 0..4 {
                        let mask = if bits == 32 {
                            u32::MAX
                        } else {
                            (1 << bits) - 1
                        };
                        let n = block * rsi * 2 + block / 2;
                        let samples: Vec<u32> = (0..n)
                            .map(|i| match shape {
                                0 => lcg(&mut seed) & mask,
                                1 => 0,
                                2 => (i as u32 / 7) & mask,
                                _ => ((i as u32 * 3) & mask) ^ (lcg(&mut seed) & 1),
                            })
                            .collect();
                        let p = params(bits, block, rsi, preprocess);
                        let mut enc = Vec::new();
                        encode(&p, &samples, &mut enc).unwrap();
                        let mut dec = Vec::new();
                        decode(&p, &enc, n, &mut dec).unwrap();
                        assert_eq!(dec, samples, "bits={bits} block={block} shape={shape}");
                    }
                }
            }
        }
    }

    #[test]
    fn truncated_input_keeps_what_was_decoded() {
        let p = params(8, 16, 4, true);
        let samples: Vec<u32> = (0..256).map(|i| (i * 37 % 256) as u32).collect();
        let mut enc = Vec::new();
        encode(&p, &samples, &mut enc).unwrap();
        let mut dec = Vec::new();
        let r = decode(&p, &enc[..enc.len() / 2], samples.len(), &mut dec);
        assert_eq!(r, Err(Error::Truncated));
        assert!(!dec.is_empty() && dec.len() < samples.len());
        assert_eq!(dec[..], samples[..dec.len()]);
    }

    #[test]
    fn szip_round_trips_partial_blocks() {
        for (bpp, ppl) in [(8u8, 1000usize), (10, 77), (16, 64)] {
            let sz = Szip {
                options: 49,
                bits_per_pixel: bpp,
                pixels_per_block: 16,
                pixels_per_scanline: ppl,
            };
            let sb = sz.sample_bytes();
            let mut seed = 3;
            let mask = (1u32 << bpp) - 1;
            let raw: Vec<u8> = (0..ppl * 3)
                .flat_map(|i| {
                    let v = ((i as u32 / 3) + (lcg(&mut seed) & 3)) & mask;
                    v.to_be_bytes()[4 - sb..].to_vec()
                })
                .collect();
            let c = sz.compress(&raw).unwrap();
            let (d, r) = sz.decompress(&c, raw.len());
            r.unwrap();
            assert_eq!(d, raw);
        }
    }
}
