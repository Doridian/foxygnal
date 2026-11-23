//! Compare with ffmpeg's decoder (`ac3`, its float one).
//!
//! ffmpeg's encoder sets every dithflag, and zero-bit mantissas then decode
//! to noise from each decoder's own random numbers, so the streams are
//! compared twice: as encoded, where only the noise differs (SNR in the
//! 40s to 70s of dB, depending on how many bins get no bits), and with every
//! dithflag cleared (the CRCs written again), where the decoders must agree
//! to rounding. More than two channels are compared with ffmpeg's own
//! downmix (`-downmix stereo`), which uses the stream's mix levels as ours
//! does; ffmpeg's `-ac 2` (libswresample) is checked too, for streams
//! encoded with the −3 dB levels it assumes.
//!
//! These need ffmpeg and skip without it. `fixtures` checks a few small
//! streams kept in `tests/data` against ffmpeg's output stored beside them,
//! so the comparison also runs where ffmpeg isn't installed;
//! `cargo test -p foxygnal-ac3 -- --ignored write_fixtures` remakes them.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::decoder::{Mark, crc1_end, crc16};
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
    let d = std::env::temp_dir().join(format!("foxygnal-ac3-{}-{name}", std::process::id()));
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

fn floats(raw: &[u8]) -> Vec<f32> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|&b| f32::from_le_bytes(b))
        .collect()
}

/// ffmpeg's decoding, interleaved f32, downmixed by its decoder to stereo
/// when there are more channels (mono stays mono).
fn ffmpeg_decode(file: &Path) -> Vec<f32> {
    floats(&run(Command::new("ffmpeg")
        .args(["-v", "error", "-downmix", "stereo", "-f", "ac3", "-i"])
        .arg(file)
        .args(["-f", "f32le", "-"])))
}

/// ffmpeg's decoding through libswresample's `-ac 2` downmix.
fn ffmpeg_ac2(file: &Path) -> Vec<f32> {
    floats(&run(Command::new("ffmpeg")
        .args(["-v", "error", "-f", "ac3", "-i"])
        .arg(file)
        .args(["-ac", "2", "-f", "f32le", "-"])))
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

/// Each frame of a stream: (offset, length).
fn frames(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(h) = Header::parse(&data[at..]) {
        out.push((at, h.frame_len));
        at += h.frame_len;
        if at >= data.len() {
            break;
        }
    }
    out
}

/// Write both CRCs of a frame again. CRC2 ends the frame; CRC1 starts its
/// part (after the sync word), so it is the value that makes that part's
/// remainder zero: the CRC is linear, so solve for it bit by bit.
pub(crate) fn fix_crcs(frame: &mut [u8]) {
    let len = frame.len();
    let f58 = crc1_end(len);
    frame[2] = 0;
    frame[3] = 0;
    let target = crc16(&frame[2..f58]);
    let mut zeros = vec![0u8; f58 - 2];
    let mut basis = [0u16; 16];
    for (k, b) in basis.iter_mut().enumerate() {
        zeros[0] = (1u16 << k >> 8) as u8;
        zeros[1] = (1u16 << k) as u8;
        *b = crc16(&zeros);
    }
    let c = (0..=u16::MAX)
        .find(|&c| {
            (0..16)
                .filter(|&k| c >> k & 1 != 0)
                .fold(0, |a, k| a ^ basis[k])
                == target
        })
        .unwrap();
    frame[2..4].copy_from_slice(&c.to_be_bytes());
    let c2 = crc16(&frame[f58..len - 2]);
    frame[len - 2..].copy_from_slice(&c2.to_be_bytes());
    assert_eq!(crc16(&frame[2..f58]), 0);
    assert_eq!(crc16(&frame[f58..]), 0);
}

/// A change to a frame's bits: set one, or insert some.
#[derive(Clone, Debug)]
enum Edit {
    Set(usize, bool),
    Insert(usize, Vec<bool>),
}

/// Rewrite each frame: decode it (to find its fields), apply the edits
/// `f` asks for, and write its CRCs again. With `frmsizecod`, frames grow
/// to that size first (zeros after the audio blocks, where aux data would
/// be), which makes room for inserted fields.
fn rewrite(
    data: &[u8],
    frmsizecod: Option<u8>,
    mut f: impl FnMut(usize, &crate::Decoder) -> Vec<Edit>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut dec = crate::Decoder::new();
    let mut pcm = vec![0f32; crate::MAX_PCM];
    for (k, (at, len)) in frames(data).into_iter().enumerate() {
        let d = dec.decode(&data[at..at + len], &mut pcm).unwrap();
        assert!(!d.error);
        let mut frame = data[at..at + len - 2].to_vec();
        if let Some(code) = frmsizecod {
            frame[4] = (frame[4] & 0xC0) | code;
        }
        let new_len = Header::parse(&frame).unwrap().frame_len;
        assert!(new_len >= len);
        frame.resize(new_len, 0);
        let mut bits: Vec<bool> = (0..new_len * 8)
            .map(|p| frame[p >> 3] & (0x80 >> (p & 7)) != 0)
            .collect();
        let mut edits = f(k, &dec);
        let pos = |e: &Edit| match e {
            Edit::Set(p, _) | Edit::Insert(p, _) => *p,
        };
        edits.sort_by_key(|e| std::cmp::Reverse(pos(e)));
        let end = dec.marks.iter().find(|m| m.0 == Mark::End).unwrap().2;
        let mut inserted = 0;
        for e in edits {
            match e {
                Edit::Set(p, v) => bits[p] = v,
                Edit::Insert(p, v) => {
                    inserted += v.len();
                    bits.splice(p..p, v);
                }
            }
        }
        // Room: the audio must still end before auxdatae, crcrsv and CRC2.
        assert!(end + inserted + 18 <= new_len * 8, "frame {k}: no room");
        bits.truncate(new_len * 8);
        for b in &mut bits[new_len * 8 - 18..] {
            *b = false;
        }
        for (i, byte) in frame.iter_mut().enumerate() {
            *byte = (0..8).fold(0, |a, j| (a << 1) | u8::from(bits[i * 8 + j]));
        }
        fix_crcs(&mut frame);
        out.extend_from_slice(&frame);
    }
    out
}

/// Clear every dithflag, so zero-bit mantissas decode to zero in both decoders.
fn no_dither(dec: &crate::Decoder) -> Vec<Edit> {
    dec.marks
        .iter()
        .filter(|m| m.0 == Mark::Dithflag)
        .map(|m| Edit::Set(m.2, false))
        .collect()
}

/// The stream with every dithflag cleared.
fn without_dither(data: &[u8]) -> Vec<u8> {
    rewrite(data, None, |_, dec| no_dither(dec))
}

/// SNR in dB, and the largest difference in 16-bit steps.
fn compare(name: &str, ours: &[f32], theirs: &[f32]) -> (f64, f64) {
    assert_eq!(ours.len(), theirs.len(), "{name}: sample counts");
    let (mut sig, mut err, mut max) = (0f64, 0f64, 0f64);
    for (&a, &b) in ours.iter().zip(theirs) {
        sig += f64::from(b) * f64::from(b);
        let e = f64::from(a) - f64::from(b);
        err += e * e;
        max = max.max(e.abs());
    }
    let snr = 10.0 * (sig / err.max(1e-30)).log10();
    let lsb = max * 32768.0;
    eprintln!("{name}: SNR {snr:.1} dB, max error {lsb:.3} LSB");
    (snr, lsb)
}

/// A test signal: tones and a little noise on each channel, different per
/// channel, and an LFE tone.
fn encode(file: &Path, rate: u32, layout: &str, kbps: u32, extra: &[&str]) {
    let src = format!(
        "aevalsrc=exprs='0.3*sin(2*PI*440*t)+0.1*sin(2*PI*3300*t)+0.05*(random(0)-0.5)\
         |0.25*sin(2*PI*660*t+1)+0.1*sin(2*PI*9000*t)\
         |0.3*sin(2*PI*220*t)*sin(2*PI*2*t)\
         |0.2*sin(2*PI*1500*t)+0.02*(random(1)-0.5)\
         |0.2*sin(2*PI*5000*t)*cos(2*PI*3*t)\
         |0.4*sin(2*PI*50*t)':s={rate}:d=0.6:c=5.1"
    );
    let n = match layout {
        "mono" => 1,
        "stereo" => 2,
        "2.1" | "3.0" => 3,
        "quad" | "4.0" => 4,
        "5.0" => 5,
        _ => 6,
    };
    let pan: String = (0..n).map(|c| format!("|c{c}=c{c}")).collect();
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", &src])
        .args(["-af", &format!("pan={layout}{pan}")])
        .args(["-c:a", "ac3", "-b:a", &format!("{kbps}k")])
        .args(extra)
        .arg(file));
}

#[test]
fn ffmpeg_encoded() {
    if !have_ffmpeg() {
        return;
    }
    let d = dir("ffmpeg");
    let (mut worst, mut worst_dither) = (f64::INFINITY, f64::INFINITY);
    for (rate, layout, kbps) in [
        (48000, "mono", 96),
        (48000, "stereo", 192),
        (48000, "5.1", 448),
        (48000, "5.1", 640),
        (48000, "stereo", 32),
        (48000, "2.1", 128),
        (48000, "3.0", 256),
        (48000, "quad", 320),
        (48000, "5.0", 384),
        (44100, "stereo", 128),
        (44100, "5.1", 384),
        (44100, "mono", 48),
        (32000, "stereo", 96),
        (32000, "5.1", 320),
        (32000, "mono", 56),
    ] {
        let name = format!("{rate}-{layout}-{kbps}k");
        let file = d.join(format!("{name}.ac3"));
        encode(&file, rate, layout, kbps, &[]);
        let data = std::fs::read(&file).unwrap();
        let h = Header::parse(&data).unwrap();
        assert_eq!(h.sample_rate, rate);

        let (ours, s) = decode(&data);
        assert_eq!(s.errors(), 0, "{name}");
        let (snr, _) = compare(&format!("{name} (dithered)"), &ours, &ffmpeg_decode(&file));
        worst_dither = worst_dither.min(snr);

        let plain = d.join(format!("{name}-nodither.ac3"));
        std::fs::write(&plain, without_dither(&data)).unwrap();
        let (ours, s) = decode(&std::fs::read(&plain).unwrap());
        assert_eq!(s.errors(), 0, "{name}");
        let (snr, lsb) = compare(&name, &ours, &ffmpeg_decode(&plain));
        assert!(
            snr > 100.0 && lsb < 1.0,
            "{name}: SNR {snr:.1} dB, {lsb:.3} LSB"
        );
        worst = worst.min(snr);
    }
    eprintln!(
        "ffmpeg-encoded streams: worst SNR {worst:.1} dB without dither, {worst_dither:.1} dB as encoded"
    );
    std::fs::remove_dir_all(d).ok();
}

/// What ffmpeg's encoder writes only when asked: coupling from several
/// start bands, no rematrixing, the alternate BSI (bsid 6) with Lo/Ro mix
/// levels, and other metadata.
#[test]
fn ffmpeg_encoder_options() {
    if !have_ffmpeg() {
        return;
    }
    let d = dir("options");
    let mut worst = f64::INFINITY;
    type Case<'a> = (&'a str, u32, &'a str, u32, &'a [&'a str]);
    let cases: &[Case] = &[
        (
            "cpl1",
            48000,
            "stereo",
            96,
            &["-channel_coupling", "1", "-cpl_start_band", "1"],
        ),
        (
            "cpl2",
            48000,
            "stereo",
            128,
            &["-channel_coupling", "1", "-cpl_start_band", "2"],
        ),
        (
            "cpl9",
            44100,
            "stereo",
            160,
            &["-channel_coupling", "1", "-cpl_start_band", "9"],
        ),
        (
            "cpl51",
            48000,
            "5.1",
            384,
            &["-channel_coupling", "1", "-cpl_start_band", "5"],
        ),
        ("cpl30", 32000, "3.0", 192, &["-channel_coupling", "1"]),
        (
            "noremat",
            48000,
            "stereo",
            192,
            &["-stereo_rematrixing", "0"],
        ),
        (
            "loro",
            48000,
            "5.1",
            448,
            &[
                "-dmix_mode",
                "2",
                "-loro_cmixlev",
                "0.841",
                "-loro_surmixlev",
                "0.354",
            ],
        ),
        (
            "levels",
            48000,
            "5.0",
            320,
            &[
                "-center_mixlev",
                "0.5",
                "-surround_mixlev",
                "0",
                "-dialnorm",
                "-20",
            ],
        ),
        (
            "meta",
            44100,
            "stereo",
            128,
            &["-dsur_mode", "2", "-mixing_level", "100", "-room_type", "1"],
        ),
    ];
    for &(name, rate, layout, kbps, extra) in cases {
        let file = d.join(format!("{name}.ac3"));
        encode(&file, rate, layout, kbps, extra);
        let plain = d.join(format!("{name}-nodither.ac3"));
        std::fs::write(&plain, without_dither(&std::fs::read(&file).unwrap())).unwrap();
        let data = std::fs::read(&plain).unwrap();
        if name == "loro" {
            assert_eq!(Header::parse(&data).unwrap().bsid, 6);
        }
        let (ours, s) = decode(&data);
        assert_eq!(s.errors(), 0, "{name}");
        let (snr, lsb) = compare(name, &ours, &ffmpeg_decode(&plain));
        assert!(
            snr > 100.0 && lsb < 1.0,
            "{name}: SNR {snr:.1} dB, {lsb:.3} LSB"
        );
        worst = worst.min(snr);
    }
    eprintln!("encoder options: worst SNR {worst:.1} dB");
    std::fs::remove_dir_all(d).ok();
}

/// ffmpeg's `-ac 2` (libswresample) mixes 5.1 as L + 0.707·C + 0.707·Ls,
/// whatever the stream says, and doesn't normalise float output. Ours (and
/// ffmpeg's decoder) use the stream's levels and scale each side by
/// 1 / (1 + clev + slev) so it can't clip. With −3 dB levels in the stream
/// the two differ by that factor alone.
#[test]
fn downmix_matches_swresample() {
    if !have_ffmpeg() {
        return;
    }
    let d = dir("swr");
    for (layout, kbps) in [("5.1", 448), ("5.0", 384), ("quad", 384)] {
        let name = format!("swr-{layout}");
        let file = d.join(format!("{name}.ac3"));
        encode(
            &file,
            48000,
            layout,
            kbps,
            &["-center_mixlev", "0.707", "-surround_mixlev", "0.707"],
        );
        let plain = d.join(format!("{name}-nodither.ac3"));
        std::fs::write(&plain, without_dither(&std::fs::read(&file).unwrap())).unwrap();
        let (ours, _) = decode(&std::fs::read(&plain).unwrap());
        let h = core::f32::consts::FRAC_1_SQRT_2;
        let norm = if layout == "quad" {
            1.0 + h
        } else {
            1.0 + 2.0 * h
        };
        let ours: Vec<f32> = ours.iter().map(|&v| v * norm).collect();
        let (snr, _) = compare(&name, &ours, &ffmpeg_ac2(&plain));
        assert!(snr > 90.0, "{name}: SNR {snr:.1} dB");
    }
    std::fs::remove_dir_all(d).ok();
}

/// The fields ffmpeg's encoder never writes, put into its streams: short
/// transforms (blksw), dynamic range words, phase flags and delta bit
/// allocation. Frames are first grown to a larger frame size to make room.
/// Each change keeps the bit allocation as it was (a delta only raises the
/// mask of a band whose bins have no bits), so both decoders must still agree.
fn mutate(data: &[u8], frmsizecod: u8, seed: u64) -> Vec<u8> {
    let mut rng = seed;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    rewrite(data, Some(frmsizecod), |frame, dec| {
        let mut edits = no_dither(dec);
        // ffmpeg downmixes before its transform when every channel's is
        // the same; when they differ it rebuilds each channel's overlap
        // from the downmixed one, approximately (for stereo, which it never
        // downmixes, the first time it copies the left overlap to the right).
        // So with more than two channels a block switches all or none, and
        // two channels switch differently from the very first block, while
        // the overlaps are still zero.
        let nfch = dec
            .marks
            .iter()
            .filter(|m| m.1 == 0 && matches!(m.0, Mark::Blksw(_)))
            .count();
        let mut switch = [false; 6];
        for s in &mut switch {
            *s = next() % 3 == 0;
        }
        for &(m, blk, pos) in &dec.marks {
            match m {
                Mark::Blksw(0) if nfch == 2 && frame == 0 && blk == 0 => {
                    edits.push(Edit::Set(pos, true));
                }
                Mark::Blksw(_) if nfch == 2 && frame == 0 && blk == 0 => {}
                Mark::Blksw(_) if nfch > 2 && switch[blk] => edits.push(Edit::Set(pos, true)),
                Mark::Blksw(_) if nfch <= 2 && next() % 3 == 0 => edits.push(Edit::Set(pos, true)),
                // A dynamic range word in some blocks: ×1.5, 0.625, 2 or 0.25.
                Mark::Dynrnge if blk % 2 == 1 => {
                    let code = [0x10u8, 0xE8, 0x20, 0xC0][(next() % 4) as usize];
                    edits.push(Edit::Set(pos, true));
                    let bits = (0..8).map(|i| code >> (7 - i) & 1 != 0).collect();
                    edits.push(Edit::Insert(pos + 1, bits));
                }
                Mark::Phsflginu => edits.push(Edit::Set(pos, true)),
                Mark::Phsflg(n) => {
                    let bits = (0..n).map(|_| next() % 2 == 0).collect();
                    edits.push(Edit::Insert(pos, bits));
                }
                Mark::Deltbaie(n, first_fbw) if blk >= 4 => {
                    // A band of a full-bandwidth channel without bits here and after.
                    let found = (0..n - first_fbw).find_map(|ch| {
                        (0..28)
                            .find(|&band| dec.baps[blk..].iter().all(|b| b[ch][band] == 0))
                            .map(|band| (ch, band))
                    });
                    let Some((ch, band)) = found else {
                        continue;
                    };
                    let mut v = Vec::new();
                    let mut put =
                        |x: u32, bits: u32| v.extend((0..bits).rev().map(|i| x >> i & 1 != 0));
                    for i in 0..n {
                        put(if i == first_fbw + ch { 1 } else { 2 }, 2);
                    }
                    // One segment: at `band`, one band long, +6 dB.
                    put(0, 3);
                    put(band as u32, 5);
                    put(1, 4);
                    put(5, 3);
                    edits.push(Edit::Set(pos, true));
                    edits.push(Edit::Insert(pos + 1, v));
                }
                _ => {}
            }
        }
        edits
    })
}

#[test]
fn fields_ffmpeg_never_writes() {
    if !have_ffmpeg() {
        return;
    }
    let d = dir("mutate");
    let mut worst = f64::INFINITY;
    for (name, layout, kbps, code, extra) in [
        (
            "stereo-cpl",
            "stereo",
            128,
            28u8,
            &["-channel_coupling", "1"][..],
        ),
        ("stereo", "stereo", 192, 30, &[][..]),
        ("5.1", "5.1", 384, 36, &[][..]),
        ("mono", "mono", 96, 24, &[][..]),
        (
            "32k-stereo-cpl",
            "stereo",
            96,
            24,
            &["-channel_coupling", "1"][..],
        ),
    ] {
        let file = d.join(format!("{name}.ac3"));
        let rate = if name.starts_with("32k") {
            32000
        } else {
            48000
        };
        encode(&file, rate, layout, kbps, extra);
        let data = mutate(
            &std::fs::read(&file).unwrap(),
            code,
            0x5EED + u64::from(code),
        );
        let out = d.join(format!("{name}-mutated.ac3"));
        std::fs::write(&out, &data).unwrap();
        let (ours, s) = decode(&data);
        assert_eq!(s.errors(), 0, "{name}");
        let (snr, lsb) = compare(&format!("{name} mutated"), &ours, &ffmpeg_decode(&out));
        assert!(
            snr > 100.0 && lsb < 1.0,
            "{name}: SNR {snr:.1} dB, {lsb:.3} LSB"
        );
        worst = worst.min(snr);
    }
    eprintln!("mutated streams: worst SNR {worst:.1} dB");
    std::fs::remove_dir_all(d).ok();
}

/// A frame with a bad CRC plays as silence and is counted.
#[test]
fn crc_errors_are_silent_and_counted() {
    let mut data = std::fs::read(fixture_dir().join("48k-stereo-192k.ac3")).unwrap();
    let (clean, s) = decode(&data);
    assert_eq!((s.frames(), s.errors()), (4, 0));
    let len = Header::parse(&data).unwrap().frame_len;
    // A bit in the second frame's first part (CRC1), then one in its last (CRC2).
    data[len + 40] ^= 0x10;
    let (bad, s) = decode(&data);
    assert_eq!((s.frames(), s.errors()), (4, 1));
    let per = 1536 * 2;
    assert_eq!(bad[..per], clean[..per]);
    assert!(bad[per..2 * per].iter().all(|&v| v == 0.0));
    data[len + 40] ^= 0x10;
    data[len + len - 10] ^= 0x01;
    let (_, s) = decode(&data);
    assert_eq!(s.errors(), 1);
}

/// Damage that still passes the CRCs (they're written again after it):
/// random bits anywhere after the sync info, again and again. Never a
/// panic, never anything but finite samples.
#[test]
fn damaged_frames_never_panic() {
    let data = std::fs::read(fixture_dir().join("48k-5.1-448k.ac3")).unwrap();
    let mut rng = 0x0123_4567_89AB_CDEF_u64;
    let mut dec = crate::Decoder::new();
    let mut pcm = vec![0f32; crate::MAX_PCM];
    let (mut bad, mut total) = (0, 0);
    for (at, len) in frames(&data) {
        for _ in 0..300 {
            let mut f = data[at..at + len].to_vec();
            for _ in 0..1 + rng % 8 {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                // Not the sync info: the frame would just not be one.
                let bit = 48 + (rng as usize) % (len * 8 - 48);
                f[bit >> 3] ^= 0x80 >> (bit & 7);
            }
            fix_crcs(&mut f);
            let d = dec.decode(&f, &mut pcm).unwrap();
            assert!(pcm.iter().all(|v| v.is_finite()));
            total += 1;
            bad += usize::from(d.error);
        }
    }
    eprintln!("{bad} of {total} damaged frames failed to parse");
}

#[test]
fn eac3_is_recognised_not_decoded() {
    if !have_ffmpeg() {
        return;
    }
    let d = dir("eac3");
    let file = d.join("x.eac3");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("sine=frequency=1000:duration=0.3")
        .args(["-c:a", "eac3", "-f", "eac3"])
        .arg(&file));
    let (pcm, s) = decode(&std::fs::read(&file).unwrap());
    assert!(pcm.is_empty());
    assert_eq!(s.frames(), 0);
    assert!(s.last_header().unwrap().eac3());
    std::fs::remove_dir_all(d).ok();
}

#[test]
#[ignore = "timing; run with --release"]
fn speed() {
    let data = std::fs::read(fixture_dir().join("48k-5.1-448k.ac3")).unwrap();
    let t = std::time::Instant::now();
    let mut n = 0;
    for _ in 0..500 {
        let (_, s) = decode(&data);
        n += s.frames();
    }
    let per = t.elapsed().as_secs_f64() / f64::from(n) * 1e3;
    eprintln!("5.1 at 448 kbit/s: {per:.3} ms per frame (32 ms of sound)");
}

// ---- Fixtures ----------------------------------------------------------------

/// Small streams kept in `tests/data` (dithering off, so they decode the
/// same everywhere), each with every 7th value of ffmpeg's decoding (stereo
/// downmix) as 16-bit samples (`.ref`, little-endian).
const FIXTURES: &[&str] = &[
    "48k-stereo-192k.ac3",
    "48k-5.1-448k.ac3",
    "44k-mono-64k.ac3",
    "32k-stereo-cpl-mutated.ac3",
];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn to_i16(v: f32) -> i16 {
    (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16
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

#[test]
#[ignore = "remakes tests/data with ffmpeg"]
fn write_fixtures() {
    let d = fixture_dir();
    let tmp = dir("fixtures");
    type Spec<'a> = (&'a str, u32, &'a str, u32, &'a [&'a str], usize, Option<u8>);
    let specs: &[Spec] = &[
        ("48k-stereo-192k.ac3", 48000, "stereo", 192, &[], 4, None),
        ("48k-5.1-448k.ac3", 48000, "5.1", 448, &[], 3, None),
        ("44k-mono-64k.ac3", 44100, "mono", 64, &[], 3, None),
        (
            "32k-stereo-cpl-mutated.ac3",
            32000,
            "stereo",
            96,
            &["-channel_coupling", "1"],
            4,
            Some(24),
        ),
    ];
    for &(name, rate, layout, kbps, extra, keep, code) in specs {
        let file = tmp.join(name);
        encode(&file, rate, layout, kbps, extra);
        let data = std::fs::read(&file).unwrap();
        let data = match code {
            Some(code) => mutate(&data, code, 99),
            None => without_dither(&data),
        };
        let n: usize = frames(&data).iter().take(keep).map(|f| f.1).sum();
        std::fs::write(d.join(name), &data[..n]).unwrap();
    }
    for name in FIXTURES {
        let r: Vec<u8> = ffmpeg_decode(&d.join(name))
            .iter()
            .step_by(7)
            .flat_map(|&v| to_i16(v).to_le_bytes())
            .collect();
        std::fs::write(d.join(format!("{name}.ref")), r).unwrap();
    }
    std::fs::remove_dir_all(tmp).ok();
}
