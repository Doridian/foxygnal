//! DSP kernel benchmarks: FIR dot products, polyphase resamplers, the
//! rotator, FFT spectra and their glue. Throughput is in input samples.
//!
//! `cargo bench -p foxygnal-dsp` (add `-- --quick` for a fast pass).

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use foxygnal_dsp::{Complex32, Kernels, PowerSpectrum, Resampler, Rotator, Window, fir};

/// Samples per measured call: big enough to amortise call overhead, small
/// enough to stay in L2 like a scheduler quantum does.
const N: usize = 16_384;

/// Deterministic noise in [-1, 1).
fn noise(n: usize, seed: u32) -> Vec<f32> {
    let mut s = seed.wrapping_mul(2_654_435_761).max(1);
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s as f32 / u32::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

fn cnoise(n: usize, seed: u32) -> Vec<Complex32> {
    noise(2 * n, seed)
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| Complex32::new(p[0], p[1]))
        .collect()
}

fn taps(n: usize) -> Vec<f32> {
    noise(n, 99).iter().map(|v| v * 0.1).collect()
}

fn fir_filters(c: &mut Criterion) {
    let mut g = c.benchmark_group("fir");
    g.throughput(Throughput::Elements(N as u64));
    let xr = noise(N, 1);
    let xc = cnoise(N, 2);
    for ntaps in [31, 127, 511] {
        let h = taps(ntaps);
        let mut fr = Resampler::<f32>::fir(&h);
        let mut out = vec![0.0f32; N];
        g.bench_with_input(BenchmarkId::new("f32", ntaps), &ntaps, |b, _| {
            b.iter(|| fr.process(black_box(&xr), &mut out))
        });
        let mut fc = Resampler::<Complex32>::fir(&h);
        let mut outc = vec![Complex32::default(); N];
        g.bench_with_input(BenchmarkId::new("c32", ntaps), &ntaps, |b, _| {
            b.iter(|| fc.process(black_box(&xc), &mut outc))
        });
    }
    g.finish();
}

fn resamplers(c: &mut Criterion) {
    let mut g = c.benchmark_group("resampler");
    g.throughput(Throughput::Elements(N as u64));
    let xc = cnoise(N, 3);
    let xr = noise(N, 4);

    // 2.4 MS/s → 240 kS/s channel decimator (complex).
    let h = fir::resampler_lowpass(1, 10, 0.2).design().unwrap();
    let mut r = Resampler::<Complex32>::new(1, 10, &h);
    let mut out = vec![Complex32::default(); N];
    g.bench_function(format!("decim10_c32_{}taps", h.len()), |b| {
        b.iter(|| r.process(black_box(&xc), &mut out))
    });

    // The FM receiver's channel filter as `freq_xlating_fir` runs it: mix a
    // chunk with the rotator, then filter and decimate (198 Hamming taps).
    let h = fir::FirSpec {
        band: fir::Band::LowPass(100e3 / 2.4e6),
        transition: 40e3 / 2.4e6,
        window: fir::DesignWindow::Hamming,
        gain: 1.0,
    }
    .design()
    .unwrap();
    let mut r = Resampler::<Complex32>::new(1, 10, &h);
    let mut rot = Rotator::new(-300e3 / 2.4e6);
    let mut mixed = vec![Complex32::default(); 4096];
    g.bench_function(format!("xlate_dec10_c32_{}taps", h.len()), |b| {
        b.iter(|| {
            for (x, y) in black_box(&xc)
                .chunks(4096)
                .zip(out.chunks_mut(4096 / 10 + 1))
            {
                let m = &mut mixed[..x.len()];
                rot.rotate(x, m);
                r.process(m, y);
            }
        })
    });

    // 240 kS/s → 48 kS/s audio (the ratio reduces to ÷5).
    let h = fir::resampler_lowpass(1, 5, 0.2).design().unwrap();
    let mut r = Resampler::<f32>::new(1, 5, &h);
    let mut outr = vec![0.0f32; N];
    g.bench_function(format!("240k_48k_f32_{}taps", h.len()), |b| {
        b.iter(|| r.process(black_box(&xr), &mut outr))
    });

    // 48 kS/s → 44.1 kS/s, a true rational ratio (147/160).
    let h = fir::resampler_lowpass(147, 160, 0.2).design().unwrap();
    let mut r = Resampler::<f32>::new(147, 160, &h);
    g.bench_function(format!("48k_44k1_f32_{}taps", h.len()), |b| {
        b.iter(|| r.process(black_box(&xr), &mut outr))
    });
    g.finish();
}

fn rotator(c: &mut Criterion) {
    let mut g = c.benchmark_group("rotator");
    g.throughput(Throughput::Elements(N as u64));
    let xc = cnoise(N, 5);
    let mut out = vec![Complex32::default(); N];
    let mut rot = Rotator::new(0.0123);
    g.bench_function("block", |b| b.iter(|| rot.rotate(black_box(&xc), &mut out)));
    g.bench_function("next", |b| {
        b.iter(|| {
            for (o, &x) in out.iter_mut().zip(black_box(&xc)) {
                *o = rot.next(x);
            }
        })
    });
    g.finish();
}

fn spectrum(c: &mut Criterion) {
    let mut g = c.benchmark_group("spectrum");
    for size in [1024, 8192] {
        g.throughput(Throughput::Elements(size as u64));
        let x = cnoise(size, 6);
        let mut ps = PowerSpectrum::new(size, Window::BlackmanHarris);
        let mut out = vec![0.0f32; size];
        g.bench_with_input(BenchmarkId::new("power", size), &size, |b, _| {
            b.iter(|| ps.compute(black_box(&x), &mut out))
        });
    }
    g.finish();
}

/// Each raw kernel, once per kernel set the CPU supports
/// (`sse2`/`avx2+fma` on x86_64, `neon` on aarch64).
fn raw_kernels(c: &mut Criterion) {
    let xr = noise(N, 11);
    let xc = cnoise(N, 12);
    let xc2 = cnoise(N, 13);
    let bytes: Vec<u8> = xr.iter().map(|v| ((v + 1.0) * 127.5) as u8).collect();
    let shorts: Vec<i16> = xr.iter().map(|v| (v * 32767.0) as i16).collect();
    let pw: Vec<f32> = xr.iter().map(|v| v * v).collect();
    let mut yf = vec![0.0f32; N];
    let mut yc = vec![Complex32::default(); N];
    for k in Kernels::all() {
        let lv = k.level.name();
        // Dot products: throughput in taps (multiply-accumulates).
        let mut g = c.benchmark_group(format!("dot/{lv}"));
        for ntaps in [32, 128, 512] {
            g.throughput(Throughput::Elements(ntaps as u64));
            let h = taps(ntaps);
            g.bench_with_input(BenchmarkId::new("f32", ntaps), &ntaps, |b, _| {
                b.iter(|| (k.dot_f32)(black_box(&h), black_box(&xr[..ntaps])))
            });
            let h2: Vec<f32> = h.iter().flat_map(|&v| [v, v]).collect();
            g.bench_with_input(BenchmarkId::new("c32", ntaps), &ntaps, |b, _| {
                b.iter(|| (k.dot_c32)(black_box(&h2), black_box(&xr[..2 * ntaps])))
            });
        }
        g.finish();

        let mut g = c.benchmark_group(format!("kernel/{lv}"));
        g.throughput(Throughput::Elements(N as u64));
        let mut rot = Rotator::new(0.0123);
        rot.set_kernels(&k);
        g.bench_function("rotate", |b| b.iter(|| rot.rotate(black_box(&xc), &mut yc)));
        g.bench_function("mul_c32", |b| {
            b.iter(|| (k.mul_c32)(black_box(&xc), black_box(&xc2), &mut yc))
        });
        let w = Window::Hann.coefficients(N);
        g.bench_function("window_c32", |b| {
            b.iter(|| (k.mul_c32_f32)(black_box(&xc), black_box(&w), &mut yc))
        });
        g.bench_function("power", |b| {
            b.iter(|| (k.power)(black_box(&xc), 0.5, &mut yf))
        });
        g.bench_function("power_to_db", |b| {
            b.iter(|| (k.power_to_db)(black_box(&pw), &mut yf))
        });
        g.bench_function("magnitude", |b| {
            b.iter(|| (k.magnitude)(black_box(&xc), &mut yf))
        });
        g.bench_function("phase", |b| b.iter(|| (k.phase)(black_box(&xc), &mut yf)));
        g.bench_function("fm_discriminate", |b| {
            b.iter(|| (k.fm_discriminate)(Complex32::new(1.0, 0.0), black_box(&xc), 0.5, &mut yf))
        });
        g.bench_function("u8_to_f32", |b| {
            b.iter(|| (k.u8_to_f32)(black_box(&bytes), 1.0 / 127.5, -1.0, &mut yf))
        });
        g.bench_function("i16_to_f32", |b| {
            b.iter(|| (k.i16_to_f32)(black_box(&shorts), 1.0 / 32767.0, &mut yf))
        });
        g.finish();
    }
}

/// The scalar loops the kernels replaced, as a reference point.
fn scalar_reference(c: &mut Criterion) {
    let xc = cnoise(N, 12);
    let mut yf = vec![0.0f32; N];
    let mut g = c.benchmark_group("kernel/scalar");
    g.throughput(Throughput::Elements(N as u64));
    g.bench_function("magnitude_hypot", |b| {
        b.iter(|| {
            for (y, x) in yf.iter_mut().zip(black_box(&xc)) {
                *y = x.norm();
            }
        })
    });
    g.bench_function("phase_libm", |b| {
        b.iter(|| {
            for (y, x) in yf.iter_mut().zip(black_box(&xc)) {
                *y = x.arg();
            }
        })
    });
    g.bench_function("fm_discriminate_libm", |b| {
        b.iter(|| {
            let mut prev = Complex32::new(1.0, 0.0);
            for (y, &x) in yf.iter_mut().zip(black_box(&xc)) {
                *y = (x * prev.conj()).arg() * 0.5;
                prev = x;
            }
        })
    });
    g.finish();
}

criterion_group!(
    benches,
    fir_filters,
    resamplers,
    rotator,
    spectrum,
    raw_kernels,
    scalar_reference
);
criterion_main!(benches);
