//! FM stereo decoder: MPX in, left and right audio out.
//!
//! A PLL locks to the 19 kHz pilot. Twice its phase regenerates the 38 kHz
//! carrier, which brings (L−R)/2 down to baseband. The mono sum (the MPX
//! itself) and the difference go through one complex polyphase filter (sum
//! in the real part, difference in the imaginary part, so both see the same
//! delay) that low-passes at 15 kHz and resamples to the audio rate. Then
//! L = M + S, R = M − S and de-emphasis. Without a pilot both outputs carry
//! the mono sum.

use std::f32::consts::{PI, TAU};

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Band, DesignWindow, FirSpec, OnePole, Resampler};

use super::{PILOT, ratio};
use crate::design::{self, fmt_rate};
use crate::digital::loop_gains;
use crate::filters::not_configured;

/// Top of the audio band.
const AUDIO_TOP: f64 = 15_000.0;
/// The low-pass is down 60 dB here (below the pilot).
const STOP: f64 = 18_500.0;
/// Highest L−R frequency: the MPX rate must carry it.
const MPX_TOP: f64 = 2.0 * PILOT + AUDIO_TOP;
/// Outputs per inner pass (size of the scratch buffer).
const CHUNK: usize = 512;

/// PLL loop bandwidth and the phase detector's smoothing (two one-poles).
const LOOP_BW_HZ: f64 = 20.0;
const DETECTOR_HZ: f64 = 500.0;
/// Time constant of the lock detector.
const LOCK_TIME: f64 = 0.05;
/// The PLL may pull this far from 19 kHz.
const PULL_HZ: f64 = 60.0;
/// Pilot level (amplitude ÷ 2, at ±1 = full deviation) to declare / lose
/// stereo. A standard pilot (9 % of 75 kHz) reads 0.045.
const LOCK_ON: f32 = 0.01;
const LOCK_OFF: f32 = 0.006;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "fm_stereo",
            label: "FM Stereo Decoder",
            category: "Demodulators",
            help: "Broadcast FM stereo from the MPX signal: connect it to an FM Demodulator \
                   with de-emphasis off, at 106 kS/s or more (e.g. 240 kS/s). A PLL locks to \
                   the 19 kHz pilot and regenerates the 38 kHz carrier for L−R; the result is \
                   low-passed at 15 kHz, resampled to the audio rate and de-emphasized. Falls \
                   back to mono (both outputs = (L+R)/2) without a pilot, and tags `stereo` \
                   (true / false) on both outputs when that changes.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![
                PortSpec {
                    name: "left",
                    ty: Some(StreamType::F32),
                },
                PortSpec {
                    name: "right",
                    ty: Some(StreamType::F32),
                },
            ],
            params: vec![
                design::float_min(
                    "audio_rate",
                    "Audio rate",
                    48_000.0,
                    0.0,
                    Some("S/s"),
                    "Output sample rate (whole number, at least 31 kS/s). 0 keeps the MPX \
                     rate.",
                ),
                design::choice(
                    "deemphasis",
                    "De-emphasis",
                    vec!["off", "50us", "75us"],
                    "50us",
                    "Undoes broadcast pre-emphasis: 50 µs in Europe and most of the world, \
                     75 µs in the Americas and Korea.",
                ),
                design::choice(
                    "mode",
                    "Mode",
                    vec!["auto", "mono"],
                    "auto",
                    "auto: stereo while the pilot is there, mono otherwise. mono: always \
                     (L+R)/2 on both outputs (quieter on weak stations).",
                ),
            ],
        },
        build,
    );
}

/// The pilot PLL. Its NCO tracks `cos(phase)` ≈ the pilot.
struct Pilot {
    phase: f32,
    freq: f32,
    centre: f32,
    pull: f32,
    kp: f32,
    ki: f32,
    /// Detector smoothing coefficient and the I/Q states (two poles each).
    a: f32,
    i1: f32,
    i2: f32,
    q1: f32,
    q2: f32,
    /// Lock detector: slow averages of the in-phase and quadrature levels.
    al: f32,
    level: f32,
    quad: f32,
    locked: bool,
}

impl Pilot {
    fn new(rate: f64) -> Self {
        let w = |hz: f64| (std::f64::consts::TAU * hz / rate) as f32;
        let (kp, ki) = loop_gains(std::f64::consts::TAU * LOOP_BW_HZ / rate);
        Self {
            phase: 0.0,
            freq: w(PILOT),
            centre: w(PILOT),
            pull: w(PULL_HZ),
            kp: kp as f32,
            ki: ki as f32,
            a: OnePole::alpha_for(1.0 / (std::f64::consts::TAU * DETECTOR_HZ), rate),
            i1: 0.0,
            i2: 0.0,
            q1: 0.0,
            q2: 0.0,
            al: OnePole::alpha_for(LOCK_TIME, rate),
            level: 0.0,
            quad: 0.0,
            locked: false,
        }
    }

    /// Track one MPX sample; returns `2·sin(2ωt)`, the 38 kHz reference that
    /// brings (L−R)/2 to baseband (the pilot is `sin(ωt)`, so `ωt = phase +
    /// π/2` once locked).
    #[inline]
    fn step(&mut self, x: f32) -> f32 {
        let (s, c) = self.phase.sin_cos();
        let a = self.a;
        self.i1 += a * (x * c - self.i1);
        self.i2 += a * (self.i1 - self.i2);
        self.q1 += a * (-x * s - self.q1);
        self.q2 += a * (self.q1 - self.q2);
        let err = self.q2.atan2(self.i2);
        self.freq =
            (self.freq + self.ki * err).clamp(self.centre - self.pull, self.centre + self.pull);
        self.phase += self.freq + self.kp * err;
        if self.phase > PI {
            self.phase -= TAU;
        } else if self.phase < -PI {
            self.phase += TAU;
        }
        self.level += self.al * (self.i2 - self.level);
        self.quad += self.al * (self.q2.abs() - self.quad);
        if self.locked {
            self.locked = self.level > LOCK_OFF && self.level > 2.0 * self.quad;
        } else {
            self.locked = self.level > LOCK_ON && self.level > 4.0 * self.quad;
        }
        // sin(2ωt) = sin(2·phase + π) = −2·sin·cos.
        -4.0 * s * c
    }
}

#[derive(Debug, Clone, Copy)]
struct Config {
    audio_rate: f64,
    tau: Option<f64>,
    mono: bool,
}

pub struct FmStereo {
    cfg: Config,
    pilot: Option<Pilot>,
    filter: Option<Resampler<Complex32>>,
    scratch: Vec<Complex32>,
    deemph: [OnePole; 2],
    /// Stereo state of the outputs, and whether it has been tagged.
    stereo: bool,
    tagged: bool,
}

impl FmStereo {
    fn design(&self, rate: f64) -> Result<(usize, usize, Vec<f32>, f64), Error> {
        if rate < 2.0 * MPX_TOP {
            return Err(Error::Other(format!(
                "needs the FM discriminator output (MPX) at {} or more to carry L−R up to \
                 53 kHz, not {}: decimate less before the FM demodulator",
                fmt_rate(2.0 * MPX_TOP),
                fmt_rate(rate)
            )));
        }
        let out = if self.cfg.audio_rate > 0.0 {
            self.cfg.audio_rate
        } else {
            rate
        };
        let stop = STOP.min(out / 2.0);
        if stop - AUDIO_TOP < 500.0 || out > rate {
            return Err(Error::Other(format!(
                "audio rate {} must be between 31 kS/s and the {} input rate (or 0 to keep it)",
                fmt_rate(out),
                fmt_rate(rate)
            )));
        }
        let (l, m) = if self.cfg.audio_rate > 0.0 {
            ratio(rate, out)?
        } else {
            (1, 1)
        };
        let fast = rate * l as f64;
        let taps = FirSpec {
            band: Band::LowPass((AUDIO_TOP + stop) / 2.0 / fast),
            transition: (stop - AUDIO_TOP) / fast,
            window: DesignWindow::Kaiser { attenuation: 60.0 },
            gain: l as f64,
        }
        .design()
        .map_err(|e| design::design_error(e, fast))?;
        Ok((l, m, taps, out))
    }
}

impl Block for FmStereo {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        let (l, m, taps, out) = self.design(rate)?;
        self.filter = Some(Resampler::new(l, m, &taps));
        self.pilot = Some(Pilot::new(rate));
        self.scratch = vec![Complex32::default(); CHUNK];
        let alpha = self.cfg.tau.map_or(1.0, |t| OnePole::alpha_for(t, out));
        self.deemph = [OnePole::new(alpha); 2];
        for o in cx.outputs.iter_mut() {
            o.sample_rate = Some(out);
            o.center_freq = None;
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let filter = self.filter.as_mut().ok_or_else(not_configured)?;
        let pilot = self.pilot.as_mut().ok_or_else(not_configured)?;
        let inp = io.inputs[0].slice::<f32>();
        let (lo, ro) = io.outputs.split_at_mut(1);
        let (lo, ro) = (&mut lo[0], &mut ro[0]);
        let room = lo.slice_mut::<f32>().len().min(ro.slice_mut::<f32>().len());
        let (mut used, mut made) = (0, 0);
        loop {
            let want = !self.cfg.mono && pilot.locked;
            if want != self.stereo || !self.tagged {
                self.stereo = want;
                self.tagged = true;
                lo.add_tag(made, "stereo", want);
                ro.add_tag(made, "stereo", want);
            }
            let n = (room - made).min(CHUNK);
            let (u, k) = filter.process_with(&inp[used..], &mut self.scratch[..n], |x| {
                let sub = pilot.step(x);
                Complex32::new(x, x * sub)
            });
            let g = if self.stereo { 1.0 } else { 0.0 };
            let [dl, dr] = &mut self.deemph;
            let left = &mut lo.slice_mut::<f32>()[made..made + k];
            for (o, z) in left.iter_mut().zip(&self.scratch[..k]) {
                *o = dl.next(z.re + g * z.im);
            }
            let right = &mut ro.slice_mut::<f32>()[made..made + k];
            for (o, z) in right.iter_mut().zip(&self.scratch[..k]) {
                *o = dr.next(z.re - g * z.im);
            }
            used += u;
            made += k;
            if u == 0 && k == 0 {
                break;
            }
        }
        if used == 0 && made == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        io.produce(1, made);
        Ok(WorkStatus::Ok)
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let tau = match p.str("deemphasis")? {
        "50us" => Some(50e-6),
        "75us" => Some(75e-6),
        _ => None,
    };
    Ok(Instance {
        block: Box::new(FmStereo {
            cfg: Config {
                audio_rate: p.f64("audio_rate")?,
                tau,
                mono: p.str("mode")? == "mono",
            },
            pilot: None,
            filter: None,
            scratch: Vec::new(),
            deemph: [OnePole::new(1.0); 2],
            stereo: false,
            tagged: false,
        }),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![
            ("left".into(), StreamType::F32),
            ("right".into(), StreamType::F32),
        ],
    })
}
