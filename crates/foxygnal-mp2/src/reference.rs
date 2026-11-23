//! Compare with ffmpeg's decoder (its float one, `mp1float` / `mp2float`):
//! streams ffmpeg encodes (sine tones at every sample rate it can encode,
//! mono and stereo), and random streams written here that cover what
//! ffmpeg's encoder never writes: joint stereo at every bound, dual channel,
//! Layer I, CRCs, MPEG-2.5 and every bit rate.
//!
//! These need ffmpeg and skip without it. `fixtures` checks a few small
//! streams kept in `tests/data` against ffmpeg's output stored beside them,
//! so the comparison also runs where ffmpeg isn't installed;
//! `cargo test -p foxygnal-mp2 -- --ignored write_fixtures` remakes them.

// The writers below follow the standard's loops over subbands and channels.
#![allow(clippy::needless_range_loop)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::frame::crc16;
use crate::tables::{ALLOC, CODE_BITS, STEPS, alloc_table};
use crate::{Header, Stream};

fn have_ffmpeg() -> bool {
    let ok = Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !ok {
        eprintln!("ffmpeg not found: skipping");
    }
    ok
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("foxygnal-mp2-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().expect("running ffmpeg");
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// ffmpeg's decoding, interleaved f32.
fn ffmpeg_decode(file: &Path, layer: u8) -> Vec<f32> {
    let raw = run(Command::new("ffmpeg")
        .args(["-v", "error", "-f", "mp3", "-c:a"])
        .arg(if layer == 1 { "mp1float" } else { "mp2float" })
        .arg("-i")
        .arg(file)
        .args(["-f", "f32le", "-"]));
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|&b| f32::from_le_bytes(b))
        .collect()
}

/// Ours, fed in odd-sized pieces with a flush at the end.
fn decode(data: &[u8]) -> (Vec<f32>, Stream) {
    let mut s = Stream::new();
    let mut out = Vec::new();
    for chunk in data.chunks(1000) {
        let mut at = 0;
        while at < chunk.len() {
            at += s.push(&chunk[at..], None);
            while let Some(f) = s.decode() {
                out.extend_from_slice(f.pcm);
            }
        }
    }
    s.flush();
    while let Some(f) = s.decode() {
        out.extend_from_slice(f.pcm);
    }
    (out, s)
}

/// SNR in dB (`None` below −54 dBFS, where the absolute error, a few
/// thousandths of a 16-bit step from ffmpeg's fixed-point dequantisation,
/// is checked instead) and the largest difference in 16-bit steps.
fn compare(name: &str, ours: &[f32], theirs: &[f32]) -> (Option<f64>, f64) {
    assert_eq!(ours.len(), theirs.len(), "{name}: sample counts");
    let (mut sig, mut err, mut max) = (0f64, 0f64, 0f64);
    for (&a, &b) in ours.iter().zip(theirs) {
        sig += f64::from(b) * f64::from(b);
        let e = f64::from(a) - f64::from(b);
        err += e * e;
        max = max.max(e.abs());
    }
    let snr = 10.0 * (sig / err.max(1e-30)).log10();
    let rms = (sig / theirs.len().max(1) as f64).sqrt() * 32768.0;
    let lsb = max * 32768.0;
    eprintln!("{name}: SNR {snr:.1} dB, max error {lsb:.3} LSB, RMS {rms:.1} LSB");
    assert!(lsb < 1.0, "{name}: max error {lsb:.3} LSB");
    if rms < 64.0 {
        assert!(lsb < 0.05, "{name}: max error {lsb:.3} LSB");
        return (None, lsb);
    }
    assert!(snr > 90.0, "{name}: SNR {snr:.1} dB");
    (Some(snr), lsb)
}

#[test]
fn ffmpeg_encoded() {
    if !have_ffmpeg() {
        return;
    }
    let d = dir("ffmpeg");
    let mut worst: f64 = f64::INFINITY;
    for (rate, kbps) in [
        (48000, 192),
        (48000, 384),
        (48000, 32),
        (44100, 128),
        (44100, 256),
        (32000, 96),
        (32000, 56),
        (24000, 64),
        (22050, 160),
        (16000, 32),
        (16000, 8),
    ] {
        for ch in [1, 2] {
            let name = format!("{rate}-{kbps}k-{ch}ch");
            let file = d.join(format!("{name}.mp2"));
            run(Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg(format!(
                    "sine=frequency=1000:sample_rate={rate}:duration=1,volume=4"
                ))
                .args(["-f", "lavfi", "-i"])
                .arg(format!(
                    "sine=frequency=330:sample_rate={rate}:duration=1,volume=3"
                ))
                .args(["-filter_complex", "[0][1]amerge=inputs=2"])
                .args(["-ac", &ch.to_string(), "-c:a", "mp2", "-b:a"])
                .arg(format!("{kbps}k"))
                .arg(&file));
            let data = std::fs::read(&file).unwrap();
            let (ours, s) = decode(&data);
            assert_eq!(s.errors(), 0, "{name}");
            let h = Header::parse(&data).unwrap();
            assert_eq!((h.sample_rate, h.channels()), (rate, ch));
            let (snr, _) = compare(&name, &ours, &ffmpeg_decode(&file, 2));
            worst = worst.min(snr.unwrap());
        }
    }
    eprintln!("ffmpeg-encoded streams: worst SNR {worst:.1} dB");
    std::fs::remove_dir_all(d).ok();
}

// ---- Random streams ----------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 32) as u32
    }
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

struct Writer {
    buf: Vec<u8>,
    bits: usize,
    /// Bits after the CRC word that it covers.
    protected: usize,
}

impl Writer {
    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            if self.bits.is_multiple_of(8) {
                self.buf.push(0);
            }
            if (v >> i) & 1 != 0 {
                *self.buf.last_mut().unwrap() |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }
}

/// What a random stream looks like.
#[derive(Clone, Copy, Debug)]
struct Kind {
    /// 3 MPEG-1, 2 MPEG-2, 0 MPEG-2.5 (the header's bits).
    version: u8,
    layer: u8,
    bitrate_index: u8,
    rate_index: u8,
    /// 0 stereo, 1 joint, 2 dual, 3 mono.
    mode: u8,
    mode_ext: u8,
    crc: bool,
}

/// `frames` frames of random (but valid) contents.
fn random_stream(k: Kind, frames: usize, rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for f in 0..frames {
        let padding = f % 3 == 1;
        let hdr = [
            0xFF,
            0xE0 | (k.version << 3) | ((4 - k.layer) << 1) | u8::from(!k.crc),
            (k.bitrate_index << 4) | (k.rate_index << 2) | (u8::from(padding) << 1),
            (k.mode << 6) | (k.mode_ext << 4),
        ];
        let h = Header::parse(&hdr).unwrap();
        let len = h.frame_len();
        let nch = h.channels();
        let mut w = Writer {
            buf: Vec::new(),
            bits: 0,
            protected: 0,
        };
        for b in hdr {
            w.put(b.into(), 8);
        }
        if k.crc {
            w.put(0, 16);
        }
        let budget = len * 8;
        if k.layer == 1 {
            layer1(&mut w, &h, nch, budget, rng);
        } else {
            layer2(&mut w, &h, nch, budget, rng);
        }
        // Ancillary data: random bytes.
        while w.buf.len() < len {
            w.put(rng.below(256), 8);
        }
        assert_eq!(w.buf.len(), len, "{k:?}");
        if k.crc {
            let c = crc16(0xFFFF, &w.buf, 16, 16);
            let c = crc16(c, &w.buf, 48, w.protected);
            w.buf[4..6].copy_from_slice(&c.to_be_bytes());
        }
        out.extend_from_slice(&w.buf);
    }
    out
}

fn scalefactor(rng: &mut Rng) -> u32 {
    6 + rng.below(57)
}

/// Writes a Layer I frame's body, within `budget` bits.
fn layer1(w: &mut Writer, h: &Header, nch: usize, budget: usize, rng: &mut Rng) {
    let bound = if h.mode == crate::Mode::JointStereo {
        4 * (h.mode_ext as usize + 1)
    } else {
        32
    };
    let mut alloc = [[0u32; 32]; 2];
    for sb in 0..32 {
        for ch in 0..nch {
            alloc[ch][sb] = if sb >= bound && ch == 1 {
                alloc[0][sb]
            } else if rng.below(4) == 0 {
                0
            } else {
                rng.below(15)
            };
        }
    }
    let size = |alloc: &[[u32; 32]; 2]| {
        let mut bits = w.bits + 4 * (nch * bound + 32 - bound);
        for sb in 0..32 {
            for ch in 0..nch {
                let a = alloc[ch][sb] as usize;
                if a != 0 {
                    bits += 6;
                    if sb < bound || ch == 0 {
                        bits += 12 * (a + 1);
                    }
                }
            }
        }
        bits
    };
    while size(&alloc) > budget {
        let sb = rng.below(32) as usize;
        alloc[0][sb] = 0;
        if nch == 2 && (sb >= bound || rng.below(2) == 0) {
            alloc[1][sb] = 0;
        }
    }
    let start = w.bits;
    for sb in 0..32 {
        for ch in 0..nch {
            if sb < bound || ch == 0 {
                w.put(alloc[ch][sb], 4);
            }
        }
    }
    w.protected = w.bits - start;
    for sb in 0..32 {
        for ch in 0..nch {
            if alloc[ch][sb] != 0 {
                w.put(scalefactor(rng), 6);
            }
        }
    }
    for _ in 0..12 {
        for sb in 0..32 {
            for ch in 0..nch {
                let a = alloc[ch][sb];
                if a != 0 && (sb < bound || ch == 0) {
                    w.put(rng.below((1 << (a + 1)) - 1), a + 1);
                }
            }
        }
    }
}

/// Writes a Layer II frame's body, within `budget` bits.
fn layer2(w: &mut Writer, h: &Header, nch: usize, budget: usize, rng: &mut Rng) {
    let table = ALLOC[alloc_table(h.lsf(), h.sample_rate, h.bitrate / 1000 / nch as u32)];
    let mut classes: Vec<&[u8]> = Vec::new();
    for &(n, c) in table {
        classes.extend(std::iter::repeat_n(c, n as usize));
    }
    let sblimit = classes.len();
    let nbal = |sb: usize| (classes[sb].len() + 1).trailing_zeros();
    let bound = if h.mode == crate::Mode::JointStereo {
        (4 * (h.mode_ext as usize + 1)).min(sblimit)
    } else {
        sblimit
    };
    let mut alloc = [[0usize; 32]; 2];
    let mut scfsi = [[0u32; 32]; 2];
    for sb in 0..sblimit {
        for ch in 0..nch {
            alloc[ch][sb] = if sb >= bound && ch == 1 {
                alloc[0][sb]
            } else if rng.below(4) == 0 {
                0
            } else {
                rng.below(1 << nbal(sb)) as usize
            };
            scfsi[ch][sb] = rng.below(4);
        }
    }
    let code_bits = |sb: usize, a: usize| {
        let b = CODE_BITS[classes[sb][a - 1] as usize];
        if b < 0 { (-b) as usize } else { 3 * b as usize }
    };
    let size = |alloc: &[[usize; 32]; 2]| {
        let mut bits = w.bits;
        for sb in 0..sblimit {
            bits += nbal(sb) as usize * if sb < bound { nch } else { 1 };
            for ch in 0..nch {
                let a = alloc[ch][sb];
                if a != 0 {
                    bits += 2 + [18, 12, 6, 12][scfsi[ch][sb] as usize];
                    if sb < bound || ch == 0 {
                        bits += 12 * code_bits(sb, a);
                    }
                }
            }
        }
        bits
    };
    while size(&alloc) > budget {
        let sb = rng.below(sblimit as u32) as usize;
        alloc[0][sb] = 0;
        if nch == 2 && (sb >= bound || rng.below(2) == 0) {
            alloc[1][sb] = 0;
        }
    }
    let start = w.bits;
    for sb in 0..sblimit {
        for ch in 0..nch {
            if sb < bound || ch == 0 {
                w.put(alloc[ch][sb] as u32, nbal(sb));
            }
        }
    }
    for sb in 0..sblimit {
        for ch in 0..nch {
            if alloc[ch][sb] != 0 {
                w.put(scfsi[ch][sb], 2);
            }
        }
    }
    w.protected = w.bits - start;
    for sb in 0..sblimit {
        for ch in 0..nch {
            if alloc[ch][sb] != 0 {
                let n = [3, 2, 1, 2][scfsi[ch][sb] as usize];
                for _ in 0..n {
                    w.put(scalefactor(rng), 6);
                }
            }
        }
    }
    for _ in 0..12 {
        for sb in 0..sblimit {
            for ch in 0..nch {
                let a = alloc[ch][sb];
                if a == 0 || (sb >= bound && ch == 1) {
                    continue;
                }
                let q = classes[sb][a - 1] as usize;
                let steps = u32::from(STEPS[q]);
                let b = CODE_BITS[q];
                if b < 0 {
                    w.put(rng.below(steps * steps * steps), (-b) as u32);
                } else {
                    for _ in 0..3 {
                        w.put(rng.below(steps), b as u32);
                    }
                }
            }
        }
    }
}

#[test]
fn random_streams() {
    if !have_ffmpeg() {
        return;
    }
    let d = dir("random");
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    let mut worst: f64 = f64::INFINITY;
    let mut worst_lsb: f64 = 0.0;
    let mut n = 0;
    for version in [3u8, 2, 0] {
        for layer in [1u8, 2] {
            for rate_index in 0..3u8 {
                for bitrate_index in 1..15u8 {
                    // Every mode and bound, spread over the bit rates.
                    let m = (bitrate_index as usize + rate_index as usize * 5) % 7;
                    let (mode, mode_ext) =
                        [(0, 0), (1, 0), (1, 1), (1, 2), (1, 3), (2, 0), (3, 0)][m];
                    let k = Kind {
                        version,
                        layer,
                        bitrate_index,
                        rate_index,
                        mode,
                        mode_ext,
                        crc: (bitrate_index + rate_index) % 2 == 0,
                    };
                    let data = random_stream(k, 6, &mut rng);
                    let file = d.join(format!("r{n}.mp{layer}"));
                    n += 1;
                    std::fs::write(&file, &data).unwrap();
                    let (ours, s) = decode(&data);
                    assert_eq!(s.errors(), 0, "{k:?}");
                    assert_eq!(s.frames(), 6, "{k:?}");
                    let theirs = ffmpeg_decode(&file, layer);
                    let (snr, lsb) = compare(&format!("{k:?}"), &ours, &theirs);
                    worst = worst.min(snr.unwrap_or(f64::INFINITY));
                    worst_lsb = worst_lsb.max(lsb);
                }
            }
        }
    }
    eprintln!("{n} random streams: worst SNR {worst:.1} dB, max error {worst_lsb:.3} LSB");
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn crc_errors_are_silent_and_counted() {
    let mut rng = Rng(42);
    let k = Kind {
        version: 3,
        layer: 2,
        bitrate_index: 10,
        rate_index: 1,
        mode: 1,
        mode_ext: 2,
        crc: true,
    };
    let mut data = random_stream(k, 4, &mut rng);
    let (clean, s) = decode(&data);
    assert_eq!((s.frames(), s.errors()), (4, 0));
    assert!(clean.iter().any(|&v| v != 0.0));
    // Flip a bit in the second frame's allocation.
    let len = Header::parse(&data).unwrap().frame_len();
    data[len + 7] ^= 0x10;
    let (bad, s) = decode(&data);
    assert_eq!((s.frames(), s.errors()), (4, 1));
    let per = 1152 * 2;
    assert_eq!(bad[..per], clean[..per]);
    // The second frame is the first one's tail through the filter, then silence.
    assert!(bad[per + 1024..2 * per].iter().all(|&v| v.abs() < 1e-6));
}

// ---- Fixtures ----------------------------------------------------------------

/// Small streams kept in `tests/data`, each with every 7th value of
/// ffmpeg's decoding as 16-bit samples (`.ref`, little-endian).
const FIXTURES: &[&str] = &[
    "48k-stereo-192k.mp2",
    "22k-mono-64k.mp2",
    "random-joint-crc.mp2",
    "random-layer1-joint.mp1",
    "random-mpeg25-dual.mp2",
];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

#[test]
fn fixtures() {
    for name in FIXTURES {
        let data = std::fs::read(fixture_dir().join(name)).unwrap();
        let reference = std::fs::read(fixture_dir().join(format!("{name}.ref"))).unwrap();
        let (ours, s) = decode(&data);
        assert_eq!(s.errors(), 0, "{name}");
        let want: Vec<i16> = reference
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect();
        let got: Vec<i16> = ours.iter().step_by(7).map(|&v| to_i16(v)).collect();
        assert_eq!(got.len(), want.len(), "{name}");
        let max = got
            .iter()
            .zip(&want)
            .map(|(&a, &b)| (i32::from(a) - i32::from(b)).abs())
            .max()
            .unwrap();
        eprintln!("{name}: max error {max} LSB over {} samples", got.len());
        assert!(max <= 1, "{name}: max error {max} LSB");
    }
}

fn to_i16(v: f32) -> i16 {
    (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

#[test]
#[ignore = "remakes tests/data with ffmpeg"]
fn write_fixtures() {
    let d = fixture_dir();
    let tone = |name: &str, args: &[&str]| {
        run(Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .args(args)
            .args(["-fflags", "+bitexact", "-c:a", "mp2"])
            .arg(d.join(name)));
    };
    tone(
        "48k-stereo-192k.mp2",
        &[
            "sine=frequency=1000:sample_rate=48000:duration=0.07,volume=4",
            "-ac",
            "2",
            "-b:a",
            "192k",
        ],
    );
    tone(
        "22k-mono-64k.mp2",
        &[
            "sine=frequency=440:sample_rate=22050:duration=0.15,volume=4",
            "-b:a",
            "64k",
        ],
    );
    let mut rng = Rng(7);
    let kind = |version, layer, bitrate_index, rate_index, mode, mode_ext, crc| Kind {
        version,
        layer,
        bitrate_index,
        rate_index,
        mode,
        mode_ext,
        crc,
    };
    for (name, k) in [
        ("random-joint-crc.mp2", kind(3, 2, 6, 1, 1, 1, true)),
        ("random-layer1-joint.mp1", kind(3, 1, 4, 2, 1, 3, false)),
        ("random-mpeg25-dual.mp2", kind(0, 2, 3, 0, 2, 0, true)),
    ] {
        std::fs::write(d.join(name), random_stream(k, 3, &mut rng)).unwrap();
    }
    for name in FIXTURES {
        let file = d.join(name);
        let layer = Header::parse(&std::fs::read(&file).unwrap()).unwrap().layer;
        let r: Vec<u8> = ffmpeg_decode(&file, layer)
            .iter()
            .step_by(7)
            .flat_map(|&v| to_i16(v).to_le_bytes())
            .collect();
        std::fs::write(d.join(format!("{name}.ref")), r).unwrap();
    }
}
