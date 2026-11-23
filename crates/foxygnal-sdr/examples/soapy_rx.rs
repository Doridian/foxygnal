//! Receive-only smoke test against real hardware.
//!
//! ```sh
//! cargo run -p foxygnal-sdr --example soapy_rx -- "soapy:driver=plutosdr,uri=ip:192.168.2.1" [freq_hz] [rate]
//! ```
//!
//! Opens the device, streams for a while, measures the achieved sample rate
//! and overflows, retunes and changes gain live, and prints a crude spectrum
//! after each step. Works with any device string (rtlsdr:, hackrf:, soapy:).
//! It never transmits.

use std::time::{Duration, Instant};

use foxygnal_sdr::{Complex32, Gain, RxConfig, RxStream, default_drivers};
use rustfft::FftPlanner;

const FFT: usize = 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let device = args.next().unwrap_or_default();
    let freq: f64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(100e6);
    let rate: f64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(2.4e6);

    let drivers = default_drivers();
    let t = Instant::now();
    for d in drivers.discover() {
        println!("discover {}: {:?} {:?}", d.driver, d.devices, d.error);
    }
    println!("discovery took {:?}", t.elapsed());
    let t = Instant::now();
    let mut dev = drivers.open(&device)?;
    println!("opened {:?} in {:?}", dev.info(), t.elapsed());
    let caps = dev.capabilities();
    println!("capabilities: {caps:#?}");

    let t = Instant::now();
    let mut rx = dev.start_rx(&RxConfig {
        sample_rate: rate,
        frequency: freq,
        gain: Gain::Db(40.0),
        ppm: 0.0,
        bandwidth: None,
        antenna: None,
        channel: 0,
    })?;
    rx.set_waker(std::thread::current());
    println!("stream started in {:?}", t.elapsed());

    let mut spec = Spectrum::new();
    measure(
        &mut *rx,
        rate,
        Duration::from_millis(1500),
        "40 dB",
        freq,
        &mut spec,
    )?;

    // A few retunes first, timed (the first one may include calibration).
    for f in [freq + 0.2e6, freq - 0.2e6, freq] {
        let t = Instant::now();
        rx.set_frequency(f)?;
        println!("retuned to {:.3} MHz in {:?}", f / 1e6, t.elapsed());
        std::thread::sleep(Duration::from_millis(50));
        let mut b = vec![Complex32::default(); 1 << 16];
        while rx.read(&mut b)? > 0 {}
    }
    let f2 = freq + 0.5e6;
    let t = Instant::now();
    rx.set_frequency(f2)?;
    println!("\nretuned to {:.3} MHz in {:?}", f2 / 1e6, t.elapsed());
    measure(
        &mut *rx,
        rate,
        Duration::from_millis(700),
        "40 dB",
        f2,
        &mut spec,
    )?;

    for g in [Gain::Db(10.0), Gain::Db(60.0), Gain::Auto] {
        let t = Instant::now();
        rx.set_gain(g)?;
        println!("\ngain {g:?} set in {:?}", t.elapsed());
        measure(
            &mut *rx,
            rate,
            Duration::from_millis(700),
            &format!("{g:?}"),
            f2,
            &mut spec,
        )?;
    }

    // Control calls while streaming: 40 retunes / gain changes, reading in
    // between, timing each call and counting lost samples.
    let over0 = rx.overflows();
    let mut times = Vec::new();
    let mut buf = vec![Complex32::default(); 1 << 16];
    let start = Instant::now();
    let mut total = 0;
    for i in 0..40 {
        let t = Instant::now();
        if i % 2 == 0 {
            rx.set_frequency(freq + (i as f64 - 20.0) * 10e3)?;
        } else {
            rx.set_gain(Gain::Db(20.0 + i as f64))?;
        }
        times.push(t.elapsed());
        let until = Instant::now() + Duration::from_millis(50);
        while Instant::now() < until {
            total += rx.read(&mut buf)?;
            std::thread::park_timeout(Duration::from_millis(5));
        }
    }
    times.sort();
    println!(
        "\n40 control calls while streaming: min {:?}, median {:?}, max {:?}; \
         {:.4} MS/s meanwhile, overflows +{}",
        times[0],
        times[20],
        times[39],
        total as f64 / start.elapsed().as_secs_f64() / 1e6,
        rx.overflows() - over0
    );

    let t = Instant::now();
    drop(rx);
    println!("\nstream stopped in {:?}", t.elapsed());
    let t = Instant::now();
    drop(dev);
    println!("device closed in {:?}", t.elapsed());
    Ok(())
}

fn measure(
    rx: &mut dyn RxStream,
    rate: f64,
    dur: Duration,
    what: &str,
    centre: f64,
    spec: &mut Spectrum,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = vec![Complex32::default(); 65536];
    // Discard what's queued (settling after a change), then measure.
    let settle = Instant::now() + Duration::from_millis(100);
    while Instant::now() < settle {
        rx.read(&mut buf)?;
        std::thread::park_timeout(Duration::from_millis(5));
    }
    let over0 = rx.overflows();
    let start = Instant::now();
    let mut total = 0usize;
    let mut power = 0.0f64;
    let mut peak = 0.0f32;
    spec.reset();
    while start.elapsed() < dur {
        let n = rx.read(&mut buf)?;
        if n == 0 {
            std::thread::park_timeout(Duration::from_millis(10));
            continue;
        }
        for s in &buf[..n] {
            power += s.norm_sqr() as f64;
            peak = peak.max(s.re.abs()).max(s.im.abs());
        }
        spec.add(&buf[..n]);
        total += n;
    }
    let secs = start.elapsed().as_secs_f64();
    let achieved = total as f64 / secs;
    println!(
        "[{what}] {total} samples in {secs:.3} s = {:.4} MS/s ({:+.2}% of {:.3}); overflows +{}; \
         mean power {:.1} dBFS, peak |I/Q| {peak:.3}",
        achieved / 1e6,
        (achieved / rate - 1.0) * 100.0,
        rate / 1e6,
        rx.overflows() - over0,
        10.0 * (power / total.max(1) as f64).log10(),
    );
    spec.print(rate, centre);
    Ok(())
}

/// Averaged power spectrum.
struct Spectrum {
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    acc: Vec<f64>,
    frames: usize,
    pending: Vec<Complex32>,
}

impl Spectrum {
    fn new() -> Self {
        Spectrum {
            fft: FftPlanner::new().plan_fft_forward(FFT),
            acc: vec![0.0; FFT],
            frames: 0,
            pending: Vec::with_capacity(FFT),
        }
    }

    fn reset(&mut self) {
        self.acc.iter_mut().for_each(|a| *a = 0.0);
        self.frames = 0;
        self.pending.clear();
    }

    fn add(&mut self, mut s: &[Complex32]) {
        while !s.is_empty() {
            let take = (FFT - self.pending.len()).min(s.len());
            self.pending.extend_from_slice(&s[..take]);
            s = &s[take..];
            if self.pending.len() == FFT {
                // Hann window.
                for (i, v) in self.pending.iter_mut().enumerate() {
                    let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT as f32).cos();
                    *v *= w;
                }
                self.fft.process(&mut self.pending);
                for (a, v) in self.acc.iter_mut().zip(&self.pending) {
                    *a += v.norm_sqr() as f64;
                }
                self.frames += 1;
                self.pending.clear();
            }
        }
    }

    fn print(&self, rate: f64, centre: f64) {
        if self.frames == 0 {
            return;
        }
        // FFT-shifted dB per bin.
        let db: Vec<f64> = (0..FFT)
            .map(|i| {
                let a = self.acc[(i + FFT / 2) % FFT] / self.frames as f64 / (FFT * FFT) as f64;
                10.0 * a.max(1e-20).log10()
            })
            .collect();
        let mut sorted = db.clone();
        sorted.sort_by(f64::total_cmp);
        let floor = sorted[FFT / 2];
        // 64 columns, max per column.
        let cols = 64;
        let per = FFT / cols;
        let line: String = (0..cols)
            .map(|c| {
                let m = db[c * per..(c + 1) * per]
                    .iter()
                    .copied()
                    .fold(f64::MIN, f64::max);
                let above = m - floor;
                [' ', '.', ':', '-', '=', '+', '*', '#', '%', '@'][((above / 4.0) as usize).min(9)]
            })
            .collect();
        println!(
            "  spectrum {:.3}–{:.3} MHz (floor {floor:.1} dB/bin, each char ≈4 dB above it):",
            (centre - rate / 2.0) / 1e6,
            (centre + rate / 2.0) / 1e6
        );
        println!("  |{line}|");
        // Strongest peaks away from DC, at least 10 bins apart.
        let mut idx: Vec<usize> = (0..FFT).collect();
        idx.sort_by(|&a, &b| db[b].total_cmp(&db[a]));
        let mut peaks: Vec<usize> = Vec::new();
        for i in idx {
            if (i as isize - FFT as isize / 2).abs() < 3 {
                continue; // DC
            }
            if peaks.iter().all(|&p| (p as isize - i as isize).abs() > 10) {
                peaks.push(i);
            }
            if peaks.len() == 5 {
                break;
            }
        }
        let list: Vec<String> = peaks
            .iter()
            .map(|&i| {
                let f = centre + (i as f64 - FFT as f64 / 2.0) * rate / FFT as f64;
                format!("{:.3} MHz {:+.1} dB", f / 1e6, db[i] - floor)
            })
            .collect();
        println!("  peaks: {}", list.join(", "));
    }
}
