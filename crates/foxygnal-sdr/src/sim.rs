//! `sim`: a simulated receiver, always present, so foxygnal can be tried
//! (and tested) without hardware.
//!
//! It behaves like an RTL-SDR tuned somewhere in the FM broadcast band: a few
//! stations at fixed frequencies, each FM-modulated (75 kHz deviation) with
//! its own tone pattern, plus noise, delivered in real time at the configured
//! rate. Retuning and gain changes apply live. It has two receive channels:
//! channel 1 (alone, or in sync with channel 0 through `start_rx_dual`) is
//! channel 0 turned by 90°, as if its antenna sat a quarter wavelength
//! further from every station. It also accepts a transmit
//! stream, which takes samples at the configured rate and throws them away,
//! so the TX path can be exercised without a transmitter.
//!
//! Device string options:
//! - `stations=88.5;94.1;100.3`: station frequencies in MHz (`;`-separated),
//!   replacing the default set
//! - `noise=0.01`: noise σ per I/Q component at 30 dB gain
//!
//! The generator uses integer phase accumulators and a sine table, so it
//! never calls `sin` per sample: a few stations at 2.4 MS/s cost a small
//! fraction of a core.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use crate::ring::{self, Consumer, Producer};
use crate::{
    Capabilities, Complex32, Device, DeviceArgs, DeviceInfo, Driver, DualRxStream, Gain, Range,
    Result, RxConfig, RxStream, SdrError, TxConfig, TxStream,
};

/// A tone pattern: a cycle of (tone Hz, seconds); 0 Hz is silence.
type Pattern = &'static [(f32, f32)];

/// Default stations: (frequency, level in dBFS at 30 dB gain, tone pattern).
const STATIONS: &[(f64, f32, Pattern)] = &[
    (88.5e6, -22.0, &[(440.0, 1.0)]),
    (94.1e6, -28.0, &[(1000.0, 0.25), (0.0, 0.25)]),
    (99.1e6, -34.0, &[(600.0, 0.5), (800.0, 0.5)]),
    (
        100.3e6,
        -20.0,
        &[(700.0, 0.4), (0.0, 0.1), (1400.0, 0.2), (0.0, 0.3)],
    ),
    (100.9e6, -40.0, &[(300.0, 1.0)]),
    (
        104.7e6,
        -26.0,
        &[(523.0, 0.3), (659.0, 0.3), (784.0, 0.3), (0.0, 0.3)],
    ),
];

/// FM broadcast peak deviation.
const DEVIATION: f64 = 75e3;
/// The tone uses this much of the deviation.
const MOD_INDEX: f64 = 0.8;
const NOISE: f32 = 0.01;
/// Gain at which station levels are as listed.
const REF_GAIN: f64 = 30.0;

const TABLE_BITS: u32 = 14;
const SHIFT: u32 = 32 - TABLE_BITS;

pub struct SimDriver;

fn info() -> DeviceInfo {
    DeviceInfo {
        driver: "sim".into(),
        args: "sim:".into(),
        label: "Simulated FM band".into(),
        serial: None,
        rx: true,
        tx: true,
    }
}

impl Driver for SimDriver {
    fn name(&self) -> &'static str {
        "sim"
    }

    fn label(&self) -> &'static str {
        "Simulated receiver"
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![info()])
    }

    fn open(&self, args: &DeviceArgs) -> Result<Box<dyn Device>> {
        let mut stations: Vec<StationSpec> = STATIONS
            .iter()
            .map(|&(freq, level_db, pattern)| StationSpec {
                freq,
                level_db,
                pattern,
            })
            .collect();
        if let Some(list) = args.get("stations") {
            let mut custom = Vec::new();
            for (i, s) in list.split([';', ' ']).filter(|s| !s.is_empty()).enumerate() {
                let mhz: f64 = s.trim().parse().map_err(|_| {
                    SdrError::BadArgs(
                        args.to_string(),
                        format!("station `{s}` is not a frequency in MHz"),
                    )
                })?;
                let base = &STATIONS[i % STATIONS.len()];
                custom.push(StationSpec {
                    freq: mhz * 1e6,
                    level_db: base.1,
                    pattern: base.2,
                });
            }
            stations = custom;
        }
        let noise = args.get_parsed::<f32>("noise")?.unwrap_or(NOISE);
        let mut info = info();
        info.args = args.to_string();
        Ok(Box::new(SimDevice {
            info,
            stations: stations.into(),
            noise,
        }))
    }
}

#[derive(Clone, Copy)]
struct StationSpec {
    freq: f64,
    level_db: f32,
    pattern: Pattern,
}

struct SimDevice {
    info: DeviceInfo,
    stations: Arc<[StationSpec]>,
    noise: f32,
}

fn capabilities() -> Capabilities {
    Capabilities {
        frequency: vec![Range::new(24e6, 1766e6)],
        sample_rate: vec![Range::new(250e3, 3.2e6)],
        gain: Some(Range {
            min: 0.0,
            max: 49.6,
            step: 0.0,
        }),
        agc: true,
        antennas: vec!["RX".into()],
        bandwidth: vec![],
    }
}

/// Settings the stream changes and the generator thread picks up.
struct Control {
    freq: AtomicU64,
    /// Gain in dB as f64 bits; NaN means AGC.
    gain: AtomicU64,
    ppm: AtomicU64,
    /// Bumped on every change.
    version: AtomicU64,
    stop: AtomicBool,
}

impl Control {
    fn load(&self, a: &AtomicU64) -> f64 {
        f64::from_bits(a.load(Ordering::Relaxed))
    }

    fn store(&self, a: &AtomicU64, v: f64) {
        a.store(v.to_bits(), Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::Release);
    }
}

fn gain_bits(g: Gain) -> f64 {
    match g {
        Gain::Auto => f64::NAN,
        Gain::Db(db) => db,
    }
}

impl Device for SimDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn rx_channels(&self) -> usize {
        2
    }

    fn start_rx(&mut self, cfg: &RxConfig) -> Result<Box<dyn RxStream>> {
        let map: fn(Complex32) -> Complex32 = match cfg.channel {
            0 => |c| c,
            1 => second,
            ch => {
                return Err(SdrError::Range(format!(
                    "receive channel {ch} doesn't exist (the simulator has 0 and 1)"
                )));
            }
        };
        Ok(Box::new(self.start(cfg, map)?))
    }

    fn start_rx_dual(&mut self, cfg: &RxConfig) -> Result<Box<dyn DualRxStream>> {
        Ok(Box::new(self.start(cfg, |c| [c, second(c)])?))
    }

    fn start_tx(&mut self, cfg: &TxConfig) -> Result<Box<dyn TxStream>> {
        let caps = capabilities();
        caps.check_rate(cfg.sample_rate)?;
        caps.check_frequency(cfg.frequency)?;
        let (producer, consumer) = ring::ring((cfg.sample_rate * 0.1) as usize);
        let shared = Arc::new(TxShared {
            stop: AtomicBool::new(false),
            underflows: AtomicU64::new(0),
            taken: AtomicU64::new(0),
        });
        let thread_shared = shared.clone();
        let rate = cfg.sample_rate;
        let thread = std::thread::Builder::new()
            .name("fox:sim-tx".into())
            .spawn(move || tx_thread(rate, consumer, &thread_shared))?;
        Ok(Box::new(SimTx {
            producer,
            shared,
            thread: Some(thread),
        }))
    }
}

/// Channel 1: channel 0 turned by 90°.
fn second(c: Complex32) -> Complex32 {
    Complex32::new(-c.im, c.re)
}

impl SimDevice {
    /// Starts the generator thread; `map` makes a ring item from a sample.
    fn start<T: Copy + Default + Send + 'static>(
        &self,
        cfg: &RxConfig,
        map: fn(Complex32) -> T,
    ) -> Result<SimRx<T>> {
        let caps = capabilities();
        caps.check_rate(cfg.sample_rate)?;
        caps.check_frequency(cfg.frequency)?;
        let ctl = Arc::new(Control {
            freq: AtomicU64::new(cfg.frequency.to_bits()),
            gain: AtomicU64::new(gain_bits(cfg.gain).to_bits()),
            ppm: AtomicU64::new(cfg.ppm.to_bits()),
            version: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        // Sized like a hardware driver's ring (it used to hold 0.5 s, which
        // rounds up to 16 MiB at 2.4 MS/s).
        let (producer, consumer) = ring::ring(ring::rx_capacity(cfg.sample_rate));
        let mut gen_ = Generator::new(cfg.sample_rate, &self.stations, self.noise);
        let thread_ctl = ctl.clone();
        let thread = std::thread::Builder::new()
            .name("fox:sim-rx".into())
            .spawn(move || rx_thread(&mut gen_, producer, &thread_ctl, map))?;
        Ok(SimRx {
            consumer,
            ctl,
            thread: Some(thread),
        })
    }
}

// ---- Receive ----------------------------------------------------------------

struct SimRx<T> {
    consumer: Consumer<T>,
    ctl: Arc<Control>,
    thread: Option<JoinHandle<()>>,
}

impl<T: Copy> SimRx<T> {
    fn ended(&self, n: usize) -> Result<usize> {
        if n == 0 && self.consumer.finished() {
            return Err(SdrError::Device("the simulator stopped".into()));
        }
        Ok(n)
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        capabilities().check_frequency(freq)?;
        self.ctl.store(&self.ctl.freq, freq);
        Ok(())
    }
}

/// The parts of `RxStream` and `DualRxStream` that don't read.
macro_rules! sim_controls {
    () => {
        fn set_waker(&mut self, thread: Thread) {
            self.consumer.set_waker(thread);
        }

        fn set_frequency(&mut self, freq: f64) -> Result<()> {
            SimRx::set_frequency(self, freq)
        }

        fn set_gain(&mut self, gain: Gain) -> Result<()> {
            self.ctl.store(&self.ctl.gain, gain_bits(gain));
            Ok(())
        }

        fn set_ppm(&mut self, ppm: f64) -> Result<()> {
            self.ctl.store(&self.ctl.ppm, ppm);
            Ok(())
        }

        fn overflows(&self) -> u64 {
            self.consumer.dropped()
        }

        fn finished(&self) -> bool {
            self.consumer.finished()
        }
    };
}

impl RxStream for SimRx<Complex32> {
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize> {
        let n = self.consumer.pop(out);
        self.ended(n)
    }

    sim_controls!();
}

impl DualRxStream for SimRx<[Complex32; 2]> {
    fn read(&mut self, a: &mut [Complex32], b: &mut [Complex32]) -> Result<usize> {
        let n = self.consumer.pop_with(a.len().min(b.len()), |i, [x, y]| {
            a[i] = x;
            b[i] = y;
        });
        self.ended(n)
    }

    sim_controls!();
}

impl<T> Drop for SimRx<T> {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

/// Chunks of this long are generated and pushed at once.
const CHUNK: Duration = Duration::from_millis(10);

fn rx_thread<T: Copy + Default>(
    gen_: &mut Generator,
    mut producer: Producer<T>,
    ctl: &Control,
    map: fn(Complex32) -> T,
) {
    let chunk = ((gen_.rate * CHUNK.as_secs_f64()) as usize).max(64);
    let mut buf = vec![Complex32::default(); chunk];
    let mut items = vec![T::default(); chunk];
    let mut version = u64::MAX;
    let mut t0 = Instant::now();
    let mut sent = 0u64;
    while !ctl.stop.load(Ordering::Relaxed) && !producer.abandoned() {
        let v = ctl.version.load(Ordering::Acquire);
        if v != version {
            version = v;
            let ppm = ctl.load(&ctl.ppm);
            gen_.tune(ctl.load(&ctl.freq) * (1.0 + ppm * 1e-6));
            gen_.set_gain(ctl.load(&ctl.gain));
        }
        gen_.fill(&mut buf);
        for (d, s) in items.iter_mut().zip(&buf) {
            *d = map(*s);
        }
        producer.push(&items);
        sent += chunk as u64;
        let due = t0 + Duration::from_secs_f64(sent as f64 / gen_.rate);
        let now = Instant::now();
        if due > now {
            std::thread::park_timeout(due - now);
        } else if now - due > Duration::from_secs(1) {
            // Fell far behind (suspended?): don't try to catch up.
            t0 = now;
            sent = 0;
        }
    }
}

/// One station as the generator sees it at the current tuning.
struct Station {
    spec: StationSpec,
    amp: f32,
    /// Carrier offset from the tuned frequency, as a phase increment per sample.
    offset: i32,
    phase: u32,
    /// Peak deviation in phase increments per sample.
    dev: f32,
    audio_phase: u32,
    audio_inc: u32,
    segment: usize,
    left: u64,
}

struct Generator {
    rate: f64,
    /// e^(j2πk/N), N = 2^TABLE_BITS.
    table: Vec<Complex32>,
    all: Vec<Station>,
    /// Indices into `all` of the stations inside the passband.
    visible: Vec<usize>,
    noise: f32,
    scale: f32,
    rng: u64,
}

const PHASE_SCALE: f64 = 4_294_967_296.0;

impl Generator {
    fn new(rate: f64, stations: &[StationSpec], noise: f32) -> Self {
        let n = 1usize << TABLE_BITS;
        let table = (0..n)
            .map(|k| {
                let (s, c) = (std::f64::consts::TAU * k as f64 / n as f64).sin_cos();
                Complex32::new(c as f32, s as f32)
            })
            .collect();
        let all = stations
            .iter()
            .map(|&spec| Station {
                spec,
                amp: 10f32.powf(spec.level_db / 20.0),
                offset: 0,
                phase: 0,
                dev: (DEVIATION * MOD_INDEX / rate * PHASE_SCALE) as f32,
                audio_phase: 0,
                audio_inc: 0,
                segment: usize::MAX,
                left: 0,
            })
            .collect();
        Generator {
            rate,
            table,
            all,
            visible: Vec::with_capacity(stations.len()),
            noise,
            scale: 1.0,
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn tune(&mut self, freq: f64) {
        self.visible.clear();
        for (i, s) in self.all.iter_mut().enumerate() {
            let df = s.spec.freq - freq;
            // Past ~0.45·rate the anti-alias filter would take it.
            if df.abs() < 0.45 * self.rate {
                s.offset = (df / self.rate * PHASE_SCALE) as i64 as i32;
                self.visible.push(i);
            }
        }
    }

    fn set_gain(&mut self, db: f64) {
        let db = if db.is_nan() { REF_GAIN } else { db };
        self.scale = 10f64.powf((db - REF_GAIN) / 20.0) as f32;
    }

    fn uniform4(&mut self) -> f32 {
        // xorshift64*: four 16-bit uniforms summed ≈ Gaussian (Irwin–Hall).
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let r = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let s = (r & 0xFFFF) + ((r >> 16) & 0xFFFF) + ((r >> 32) & 0xFFFF) + (r >> 48);
        // Mean 2·65535, variance 4·65536²/12; scale to σ = 1.
        (s as f32 - 131_070.0) * (3f32.sqrt() / 65_536.0)
    }

    fn fill(&mut self, out: &mut [Complex32]) {
        let sigma = self.noise;
        for v in out.iter_mut() {
            let i = self.uniform4();
            let q = self.uniform4();
            *v = Complex32::new(i * sigma, q * sigma);
        }
        let rate = self.rate;
        for &si in &self.visible {
            let s = &mut self.all[si];
            let table = &self.table;
            let mut pos = 0;
            while pos < out.len() {
                if s.left == 0 {
                    s.segment = (s.segment.wrapping_add(1)) % s.spec.pattern.len();
                    let (hz, secs) = s.spec.pattern[s.segment];
                    s.left = ((secs as f64 * rate) as u64).max(1);
                    s.audio_inc = (hz as f64 / rate * PHASE_SCALE) as u32;
                    if hz == 0.0 {
                        s.audio_phase = 0;
                    }
                }
                let n = (out.len() - pos).min(s.left as usize);
                for v in &mut out[pos..pos + n] {
                    let m = table[(s.audio_phase >> SHIFT) as usize].im;
                    s.audio_phase = s.audio_phase.wrapping_add(s.audio_inc);
                    let inc = s.offset.wrapping_add((s.dev * m) as i32);
                    s.phase = s.phase.wrapping_add(inc as u32);
                    *v += table[(s.phase >> SHIFT) as usize] * s.amp;
                }
                s.left -= n as u64;
                pos += n;
            }
        }
        let k = self.scale;
        for v in out.iter_mut() {
            // An ADC clips.
            *v = Complex32::new((v.re * k).clamp(-1.0, 1.0), (v.im * k).clamp(-1.0, 1.0));
        }
    }
}

// ---- Transmit ---------------------------------------------------------------

struct TxShared {
    stop: AtomicBool,
    underflows: AtomicU64,
    /// Samples "transmitted" so far.
    taken: AtomicU64,
}

struct SimTx {
    producer: Producer<Complex32>,
    shared: Arc<TxShared>,
    thread: Option<JoinHandle<()>>,
}

impl TxStream for SimTx {
    fn write(&mut self, samples: &[Complex32]) -> Result<usize> {
        if self.thread.as_ref().is_none_or(|t| t.is_finished()) {
            return Err(SdrError::Device("the simulator stopped".into()));
        }
        Ok(self.producer.push_partial(samples))
    }

    fn set_waker(&mut self, thread: Thread) {
        self.producer.set_waker(thread);
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        capabilities().check_frequency(freq)
    }

    fn set_gain(&mut self, _gain: f64) -> Result<()> {
        Ok(())
    }

    fn underflows(&self) -> u64 {
        self.shared.underflows.load(Ordering::Relaxed)
    }
}

impl Drop for SimTx {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn tx_thread(rate: f64, mut consumer: Consumer<Complex32>, shared: &TxShared) {
    let chunk = ((rate * CHUNK.as_secs_f64()) as usize).max(64);
    let mut buf = vec![Complex32::default(); chunk];
    let t0 = Instant::now();
    let mut ticks = 0u32;
    let mut started = false;
    while !shared.stop.load(Ordering::Relaxed) {
        let n = consumer.pop(&mut buf);
        started |= n > 0;
        if started && n < chunk {
            shared
                .underflows
                .fetch_add((chunk - n) as u64, Ordering::Relaxed);
        }
        shared.taken.fetch_add(n as u64, Ordering::Relaxed);
        ticks += 1;
        let due = t0 + CHUNK * ticks;
        let now = Instant::now();
        if due > now {
            std::thread::park_timeout(due - now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Drivers;

    fn rx_cfg(rate: f64, freq: f64) -> RxConfig {
        RxConfig {
            sample_rate: rate,
            frequency: freq,
            gain: Gain::Db(30.0),
            ppm: 0.0,
            bandwidth: None,
            antenna: None,
            channel: 0,
        }
    }

    /// Power in a ±`bw` window around `offset` Hz, from a plain DFT.
    fn tone_power(x: &[Complex32], rate: f64, offset: f64, bw: f64) -> f64 {
        let n = x.len();
        let mut total = 0.0;
        let bins = (bw / (rate / n as f64)) as i64;
        let centre = (offset / (rate / n as f64)).round() as i64;
        for b in (centre - bins)..=(centre + bins) {
            let w = -std::f64::consts::TAU * b as f64 / n as f64;
            let (mut re, mut im) = (0.0, 0.0);
            for (k, v) in x.iter().enumerate() {
                let (s, c) = (w * k as f64).sin_cos();
                re += v.re as f64 * c - v.im as f64 * s;
                im += v.re as f64 * s + v.im as f64 * c;
            }
            total += (re * re + im * im) / (n * n) as f64;
        }
        total
    }

    #[test]
    fn stations_appear_where_tuned() {
        let rate = 1e6;
        let mut g = Generator::new(
            rate,
            &[StationSpec {
                freq: 100.3e6,
                level_db: -20.0,
                pattern: &[(1000.0, 1.0)],
            }],
            0.001,
        );
        g.tune(100.0e6);
        g.set_gain(30.0);
        let mut x = vec![Complex32::default(); 4096];
        g.fill(&mut x);
        // Carson's rule: almost all power within ±(Δf + f_m) of the carrier.
        let near = tone_power(&x, rate, 300e3, 64e3);
        let far = tone_power(&x, rate, -300e3, 64e3);
        assert!(near > 0.005, "near {near}");
        assert!(far < near / 100.0, "near {near}, far {far}");
        // Tuned away: only noise.
        g.tune(102e6);
        g.fill(&mut x);
        assert!(g.visible.is_empty());
        let p: f64 = x.iter().map(|v| v.norm_sqr() as f64).sum::<f64>() / x.len() as f64;
        assert!(p < 1e-5, "{p}");
    }

    #[test]
    fn noise_has_the_requested_sigma() {
        let mut g = Generator::new(1e6, &[], 0.1);
        let mut x = vec![Complex32::default(); 100_000];
        g.fill(&mut x);
        let var: f64 = x.iter().map(|v| (v.re as f64).powi(2)).sum::<f64>() / x.len() as f64;
        assert!((var.sqrt() - 0.1).abs() < 0.005, "{}", var.sqrt());
    }

    #[test]
    fn streams_in_real_time_and_retunes() {
        let mut drivers = Drivers::new();
        drivers.add(SimDriver);
        let mut dev = drivers.open("sim:").unwrap();
        assert_eq!(dev.info().driver, "sim");
        let err = dev.start_rx(&rx_cfg(2.4e6, 2e9)).err().unwrap();
        assert!(err.to_string().contains("out of range"), "{err}");
        let mut rx = dev.start_rx(&rx_cfg(1e6, 100e6)).unwrap();
        rx.set_waker(std::thread::current());
        let t0 = Instant::now();
        let mut got = 0;
        let mut buf = vec![Complex32::default(); 8192];
        while t0.elapsed() < Duration::from_millis(300) {
            got += rx.read(&mut buf).unwrap();
            std::thread::park_timeout(Duration::from_millis(5));
        }
        // ~300k samples; allow for slow CI machines.
        assert!((150_000..=400_000).contains(&got), "{got}");
        rx.set_frequency(94.1e6).unwrap();
        rx.set_gain(Gain::Auto).unwrap();
        assert!(rx.set_frequency(5e6).is_err());
        assert_eq!(rx.overflows(), 0);
    }

    #[test]
    fn two_channels_in_sync() {
        let mut dev = SimDriver.open(&DeviceArgs::new("sim")).unwrap();
        assert_eq!(dev.rx_channels(), 2);
        let mut cfg = rx_cfg(1e6, 100e6);
        cfg.channel = 2;
        assert!(dev.start_rx(&cfg).is_err());
        let mut rx = dev.start_rx_dual(&rx_cfg(1e6, 100e6)).unwrap();
        let (mut a, mut b) = (
            vec![Complex32::default(); 4096],
            vec![Complex32::default(); 1000],
        );
        let t0 = Instant::now();
        let mut n = 0;
        while n == 0 && t0.elapsed() < Duration::from_secs(2) {
            n = rx.read(&mut a, &mut b).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!((1..=1000).contains(&n), "{n}");
        for (x, y) in a[..n].iter().zip(&b[..n]) {
            assert_eq!(*y, Complex32::new(-x.im, x.re));
        }
        assert!(a[..n].iter().any(|x| x.norm_sqr() > 0.0));
    }

    #[test]
    fn tx_takes_samples_at_the_rate() {
        let mut dev = SimDriver.open(&DeviceArgs::new("sim")).unwrap();
        let cfg = TxConfig {
            sample_rate: 1e6,
            frequency: 433.92e6,
            gain: 0.0,
            bandwidth: None,
            antenna: None,
            channel: 0,
        };
        let mut tx = dev.start_tx(&cfg).unwrap();
        let block = vec![Complex32::default(); 1 << 20];
        // The ring holds ~0.1 s; more than that is refused until it drains.
        let first = tx.write(&block).unwrap();
        assert!(first > 0 && first < block.len(), "{first}");
        std::thread::sleep(Duration::from_millis(50));
        assert!(tx.write(&block).unwrap() > 0);
    }

    #[test]
    fn custom_stations() {
        let dev = SimDriver
            .open(&DeviceArgs::parse("sim:stations=91.0;92.5,noise=0.5").unwrap())
            .unwrap();
        assert_eq!(dev.info().args, "sim:stations=91.0;92.5,noise=0.5");
        assert!(
            SimDriver
                .open(&DeviceArgs::parse("sim:stations=abc").unwrap())
                .is_err()
        );
    }
}
