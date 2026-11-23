//! Decode streams made by ffmpeg (and mjpegtools' mpeg2enc, for field
//! pictures) and compare every picture with ffmpeg's own decoding of the
//! same stream. The IDCTs differ slightly, so pictures aren't bit-exact; a
//! correct decoder stays above 40 dB, while a wrong prediction or table
//! drops far below.
//!
//! Skipped (with a note) when the tools aren't installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use foxygnal_mpeg2::{Decoder, Picture};

fn have(tool: &str) -> bool {
    let ok = Command::new(tool)
        .arg(if tool == "ffmpeg" {
            "-version"
        } else {
            "--help"
        })
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !ok {
        eprintln!("{tool} not found: skipping");
    }
    ok
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("foxygnal-mpeg2-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("running a tool");
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// ffmpeg's decoding of `es`, as I420 frames.
fn reference(d: &Path, es: &Path) -> Vec<u8> {
    let yuv = d.join("ref.yuv");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(es)
        .args([
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&yuv));
    std::fs::read(yuv).unwrap()
}

/// Encode `frames` frames of a moving test pattern with ffmpeg.
fn ffmpeg(name: &str, size: &str, frames: u32, args: &[&str]) -> (PathBuf, PathBuf) {
    let d = dir(name);
    let es = d.join("out.m2v");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={size}:rate=25"))
        .args(["-vf", "scroll=horizontal=0.013:vertical=0.007"])
        .args(["-frames:v", &frames.to_string()])
        .args(args)
        .arg(&es));
    (d, es)
}

fn decode(es: &[u8], chunk: usize) -> (Vec<Picture>, u32) {
    let mut d = Decoder::new();
    let mut out = Vec::new();
    for c in es.chunks(chunk) {
        d.push(c, None);
        while let Some(p) = d.decode() {
            out.push(p);
        }
    }
    d.flush();
    while let Some(p) = d.decode() {
        out.push(p);
    }
    (out, d.errors())
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let se: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum();
    if se == 0.0 {
        return 99.0;
    }
    10.0 * (255.0f64.powi(2) * a.len() as f64 / se).log10()
}

/// Decode and compare; returns the worst luma and chroma PSNR. Also fails
/// on any sample that is far off, which a PSNR over a whole picture can
/// hide when a single macroblock is wrong.
fn compare(d: &Path, es: &Path, w: usize, h: usize) -> (f64, f64) {
    let want = reference(d, es);
    let bytes = std::fs::read(es).unwrap();
    let (got, errors) = decode(&bytes, 4096);
    let ysize = w * h;
    let csize = w.div_ceil(2) * h.div_ceil(2);
    let fsize = ysize + 2 * csize;
    assert_eq!(errors, 0, "decode errors");
    assert_eq!(got.len(), want.len() / fsize, "picture count");
    let (mut worst_y, mut worst_c) = (99.0f64, 99.0f64);
    for (i, p) in got.iter().enumerate() {
        assert_eq!((p.width, p.height), (w, h));
        let r = &want[i * fsize..(i + 1) * fsize];
        let worst = p.data.iter().zip(r).map(|(&a, &b)| a.abs_diff(b)).max();
        assert!(
            worst.unwrap_or(0) <= 24,
            "picture {i}: a sample is {worst:?} off"
        );
        let y = psnr(&p.data[..ysize], &r[..ysize]);
        let c = psnr(&p.data[ysize..], &r[ysize..]);
        worst_y = worst_y.min(y);
        worst_c = worst_c.min(c);
    }
    let _ = std::fs::remove_dir_all(d);
    (worst_y, worst_c)
}

fn check(name: &str, w: usize, h: usize, frames: u32, args: &[&str]) {
    if !have("ffmpeg") {
        return;
    }
    let (d, es) = ffmpeg(name, &format!("{w}x{h}"), frames, args);
    let (y, c) = compare(&d, &es, w, h);
    eprintln!("{name}: worst PSNR Y {y:.1} dB, chroma {c:.1} dB");
    assert!(
        y > 40.0 && c > 40.0,
        "{name}: Y {y:.1} dB, chroma {c:.1} dB"
    );
}

const MPEG2: &[&str] = &["-c:v", "mpeg2video", "-f", "mpeg2video"];

fn with<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut v = extra.to_vec();
    v.extend_from_slice(MPEG2);
    v
}

#[test]
fn progressive_ipb() {
    check(
        "ipb",
        352,
        288,
        30,
        &with(&["-g", "12", "-bf", "2", "-q:v", "4"]),
    );
}

#[test]
fn progressive_low_quality_large_vectors() {
    // Coarse quantisation and fast motion: escapes, large f_codes.
    let args = with(&["-g", "15", "-bf", "2", "-q:v", "20", "-me_range", "64"]);
    check("coarse", 352, 288, 20, &args);
}

#[test]
fn interlaced_frame_pictures() {
    // Field prediction and field DCT in frame pictures (most SD broadcasts).
    let args = with(&[
        "-flags",
        "+ilme+ildct",
        "-field_order",
        "tt",
        "-g",
        "12",
        "-bf",
        "2",
        "-b:v",
        "6M",
    ]);
    check("interlaced", 720, 576, 40, &args);
}

#[test]
fn alternate_scan_intra_vlc_nonlinear_quant_dc_precision() {
    let args = with(&[
        "-alternate_scan",
        "1",
        "-intra_vlc",
        "1",
        "-non_linear_quant",
        "1",
        "-intra_dc_precision",
        "2",
        "-g",
        "9",
        "-bf",
        "2",
        "-q:v",
        "6",
        "-qmax",
        "28",
    ]);
    check("alt", 352, 288, 12, &args);
}

#[test]
fn custom_matrices() {
    let intra: Vec<String> = (0..64).map(|i| (8 + i / 2).to_string()).collect();
    let inter: Vec<String> = (0..64).map(|i| (16 + i % 9).to_string()).collect();
    let (intra, inter) = (intra.join(","), inter.join(","));
    let args = with(&[
        "-intra_matrix",
        &intra,
        "-inter_matrix",
        &inter,
        "-g",
        "6",
        "-bf",
        "1",
        "-q:v",
        "5",
    ]);
    check("matrix", 352, 288, 8, &args);
}

#[test]
fn odd_size_is_cropped() {
    check(
        "odd",
        350,
        202,
        8,
        &with(&["-g", "4", "-bf", "1", "-q:v", "4"]),
    );
}

#[test]
fn high_definition() {
    // 1080 lines: coded as 1088.
    check(
        "hd",
        1920,
        1080,
        5,
        &with(&["-g", "4", "-bf", "1", "-b:v", "15M"]),
    );
}

#[test]
fn mpeg1() {
    let args = [
        "-c:v",
        "mpeg1video",
        "-g",
        "9",
        "-bf",
        "2",
        "-q:v",
        "5",
        "-f",
        "mpeg1video",
    ];
    check("mpeg1", 352, 240, 12, &args);
}

/// Interlaced frame pictures from a second encoder, mjpegtools' mpeg2enc.
/// (Its per-field mode, `-I 2`, crashes in the builds tried, so field
/// pictures are covered by `tests/synthetic.rs` instead.)
#[test]
fn mpeg2enc_interlaced() {
    if !have("ffmpeg") || !have("mpeg2enc") {
        return;
    }
    let d = dir("mpeg2enc");
    let y4m = d.join("in.y4m");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=720x576:rate=25")
        .args(["-vf", "scroll=horizontal=0.013:vertical=0.007,setfield=tff"])
        .args([
            "-frames:v",
            "12",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "yuv4mpegpipe",
        ])
        .arg(&y4m));
    let es = d.join("out.m2v");
    let input = std::fs::File::open(&y4m).unwrap();
    run(Command::new("mpeg2enc")
        .args(["-f", "3", "-b", "8000", "-I", "1", "-q", "6", "-v", "0"])
        .arg("-o")
        .arg(&es)
        .stdin(input));
    let (y, c) = compare(&d, &es, 720, 576);
    eprintln!("mpeg2enc: worst PSNR Y {y:.1} dB, chroma {c:.1} dB");
    assert!(y > 40.0 && c > 40.0, "Y {y:.1} dB, chroma {c:.1} dB");
}

#[test]
fn timestamps_follow_display_order() {
    if !have("ffmpeg") {
        return;
    }
    let (d, es) = ffmpeg("pts", "176x144", 12, &with(&["-g", "6", "-bf", "2"]));
    let bytes = std::fs::read(&es).unwrap();
    let _ = std::fs::remove_dir_all(&d);
    // One "PES packet" per picture, its PTS from its place in display
    // order: the GOP's first displayed picture plus temporal_reference.
    let starts: Vec<usize> = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, w)| w[..3] == [0, 0, 1] && (w[3] == 0 || w[3] == 0xB8))
        .map(|(i, _)| i)
        .collect();
    let pictures: Vec<usize> = starts
        .iter()
        .copied()
        .filter(|&s| bytes[s + 3] == 0)
        .collect();
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    let (mut gop_base, mut in_gop) = (0u64, 0u64);
    for (k, &s) in pictures.iter().enumerate() {
        let gop_before = starts
            .iter()
            .any(|&g| bytes[g + 3] == 0xB8 && g < s && (k == 0 || g > pictures[k - 1]));
        if gop_before {
            gop_base += in_gop;
            in_gop = 0;
        }
        in_gop += 1;
        let from = if k == 0 { 0 } else { s };
        let to = pictures.get(k + 1).copied().unwrap_or(bytes.len());
        let tr = (u64::from(bytes[s + 4]) << 2) | u64::from(bytes[s + 5] >> 6);
        dec.push(&bytes[from..to], Some(90_000 + (gop_base + tr) * 3600));
        while let Some(p) = dec.decode() {
            out.push(p.pts.unwrap());
        }
    }
    dec.flush();
    while let Some(p) = dec.decode() {
        out.push(p.pts.unwrap());
    }
    let want: Vec<u64> = (0..12).map(|i| 90_000 + i * 3600).collect();
    assert_eq!(out, want);
}

#[test]
fn damaged_streams_decode_without_panicking() {
    if !have("ffmpeg") {
        return;
    }
    let (d, es) = ffmpeg("damage", "352x288", 20, &with(&["-g", "8", "-bf", "2"]));
    let bytes = std::fs::read(&es).unwrap();
    let _ = std::fs::remove_dir_all(&d);
    let mut seed = 7u32;
    let mut rnd = |n: usize| {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as usize % n
    };
    for round in 0..20 {
        let mut b = bytes.clone();
        for _ in 0..(1 + round * 3) {
            let i = rnd(b.len());
            b[i] = rnd(256) as u8;
        }
        // Cut out a piece too.
        let at = rnd(b.len());
        let len = rnd(2000).min(b.len() - at);
        b.drain(at..at + len);
        let (pics, _) = decode(&b, 1 + rnd(5000));
        assert!(pics.len() <= 40);
    }
    // Truncated mid-picture, and after a reset.
    let mut dec = Decoder::new();
    dec.push(&bytes[..bytes.len() / 3], None);
    while dec.decode().is_some() {}
    dec.reset();
    dec.push(&bytes[bytes.len() / 2..], None);
    let mut n = 0;
    while dec.decode().is_some() {
        n += 1;
    }
    dec.flush();
    while dec.decode().is_some() {
        n += 1;
    }
    assert!(n > 0, "recovers after a reset");
}

/// Decode the file in `FOXYGNAL_M2V` and report speed and errors (writing
/// the pictures to `FOXYGNAL_YUV` if set):
/// `cargo test --release -p foxygnal-mpeg2 --test reference -- --ignored --nocapture speed`
#[test]
#[ignore]
fn speed() {
    let Ok(path) = std::env::var("FOXYGNAL_M2V") else {
        eprintln!("set FOXYGNAL_M2V to an MPEG-2 elementary stream");
        return;
    };
    let bytes = std::fs::read(path).unwrap();
    let t = std::time::Instant::now();
    let (pics, errors) = decode(&bytes, 64 << 10);
    let s = t.elapsed().as_secs_f64();
    let Some(p) = pics.first() else {
        eprintln!("no pictures, {errors} errors");
        return;
    };
    eprintln!(
        "{}x{}: {} pictures in {s:.2} s = {:.0} fps, {errors} errors",
        p.width,
        p.height,
        pics.len(),
        pics.len() as f64 / s
    );
    if let Ok(out) = std::env::var("FOXYGNAL_YUV") {
        let all: Vec<u8> = pics.iter().flat_map(|p| p.data.iter().copied()).collect();
        std::fs::write(out, all).unwrap();
    }
}
