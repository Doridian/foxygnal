//! Slicers: a pulse train and a device's timing in, rows of bits out.
//!
//! The line codes follow rtl_433's conventions, so device formats documented
//! there read the same here:
//!
//! - **PCM** (NRZ, or RZ when `long > short`): a pulse is `round(w / short)`
//!   ones; for NRZ a gap is `round(w / short)` zeros, for RZ the zeros fill the
//!   pulse's period up to `round((pulse + gap) / long)` bits.
//! - **PPM**: the gap after each pulse carries the bit, short = 0, long = 1.
//! - **PWM**: the pulse width carries the bit, short = 1, long = 0; a pulse
//!   near `sync` starts a new row and is counted.
//! - **Manchester (zero bit)**: half-bit period `short`; the first rising edge
//!   is a 0, then each mid-bit edge is a bit: rising = 0, falling = 1.
//! - **Differential Manchester**: every bit starts with an edge; two short
//!   half-bits are a 1, one long symbol a 0.
//!
//! Every slicer starts a new row at a gap longer than `gap_limit` (when set)
//! and at the end of a packet (`reset`). Widths are in microseconds.

use super::bits::BitBuffer;
use super::pulse::Pulses;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Modulation {
    OokPcm,
    OokPpm,
    OokPwm,
    OokManchester,
    OokDmc,
    FskPcm,
    FskPwm,
    FskManchester,
}

impl Modulation {
    pub fn is_fsk(self) -> bool {
        matches!(
            self,
            Modulation::FskPcm | Modulation::FskPwm | Modulation::FskManchester
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            Modulation::OokPcm => "OOK_PCM",
            Modulation::OokPpm => "OOK_PPM",
            Modulation::OokPwm => "OOK_PWM",
            Modulation::OokManchester => "OOK_MC",
            Modulation::OokDmc => "OOK_DMC",
            Modulation::FskPcm => "FSK_PCM",
            Modulation::FskPwm => "FSK_PWM",
            Modulation::FskManchester => "FSK_MC",
        }
    }
}

/// Symbol timing of a device, in microseconds (0 = not used).
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub short: f32,
    pub long: f32,
    pub sync: f32,
    pub gap_limit: f32,
    pub reset: f32,
    pub tolerance: f32,
}

/// Slice `p` with `m` and `t` into `out` (cleared first).
pub fn slice(p: &Pulses, m: Modulation, t: &Timing, out: &mut BitBuffer) {
    out.clear();
    let k = p.rate as f32 * 1e-6;
    // Timing in samples.
    let s = Timing {
        short: t.short * k,
        long: t.long * k,
        sync: t.sync * k,
        gap_limit: t.gap_limit * k,
        reset: t.reset * k,
        tolerance: t.tolerance * k,
    };
    match m {
        Modulation::OokPcm | Modulation::FskPcm => pcm(p, &s, out),
        Modulation::OokPpm => ppm(p, &s, out),
        Modulation::OokPwm | Modulation::FskPwm => pwm(p, &s, out),
        Modulation::OokManchester | Modulation::FskManchester => manchester(p, &s, out),
        Modulation::OokDmc => dmc(p, &s, out),
    }
}

/// True if the gap `g` (samples) ends a row.
#[inline]
fn row_break(g: f32, t: &Timing) -> bool {
    g > t.reset || (t.gap_limit > 0.0 && g > t.gap_limit)
}

fn pcm(p: &Pulses, t: &Timing, out: &mut BitBuffer) {
    let nrz = t.long <= t.short;
    let cap = super::bits::ROW_BITS as f32;
    for i in 0..p.n {
        let (pw, gw) = (p.pulse[i] as f32, p.gap[i] as f32);
        let ones = (pw / t.short).round().min(cap) as usize;
        if t.tolerance > 0.0 && ones > 0 && (pw - ones as f32 * t.short).abs() > t.tolerance {
            out.new_row();
            continue;
        }
        out.push_n(true, ones);
        // The last gap is the end of the train, however long it is.
        if row_break(gw, t) || i + 1 == p.n {
            out.new_row();
            continue;
        }
        let zeros = if nrz {
            (gw / t.short).round()
        } else {
            ((pw + gw) / t.long).round() - ones as f32
        };
        out.push_n(false, zeros.clamp(0.0, cap) as usize);
    }
}

fn ppm(p: &Pulses, t: &Timing, out: &mut BitBuffer) {
    let mid = 0.5 * (t.short + t.long);
    for &g in &p.gap[..p.n] {
        let g = g as f32;
        if row_break(g, t) {
            out.new_row();
            continue;
        }
        let long = g > mid;
        if t.tolerance > 0.0 {
            let nominal = if long { t.long } else { t.short };
            if (g - nominal).abs() > t.tolerance {
                out.new_row();
                continue;
            }
        }
        out.push(long);
    }
}

fn pwm(p: &Pulses, t: &Timing, out: &mut BitBuffer) {
    for i in 0..p.n {
        let (pw, gw) = (p.pulse[i] as f32, p.gap[i] as f32);
        let ds = (pw - t.short).abs();
        let dl = (pw - t.long).abs();
        if t.sync > 0.0 && (pw - t.sync).abs() < ds.min(dl) {
            out.sync();
        } else {
            let one = ds <= dl;
            if t.tolerance > 0.0 && ds.min(dl) > t.tolerance {
                out.new_row();
            } else {
                out.push(one);
            }
        }
        if row_break(gw, t) {
            out.new_row();
        }
    }
}

/// Manchester with a hard-coded leading 0: the first rising edge.
fn manchester(p: &Pulses, t: &Timing, out: &mut BitBuffer) {
    let s = t.short;
    // Time since the last mid-bit edge, or None when waiting for a rising
    // edge to start a row.
    let mut since: Option<f32> = None;
    let edge = |rising: bool, d: f32, since: &mut Option<f32>, out: &mut BitBuffer| {
        match *since {
            None => {
                if rising {
                    out.new_row();
                    out.push(false);
                    *since = Some(0.0);
                }
            }
            Some(e) => {
                let e = e + d;
                if e < 1.5 * s {
                    // A bit-boundary edge.
                    *since = Some(e);
                } else {
                    // A mid-bit edge. Like rtl_433, anything later than
                    // 1.5 half-bits counts, so long runs keep their sync.
                    out.push(!rising);
                    *since = Some(0.0);
                }
            }
        }
    };
    for i in 0..p.n {
        let (pw, gw) = (p.pulse[i] as f32, p.gap[i] as f32);
        if pw > 0.0 {
            edge(true, 0.0, &mut since, out);
            edge(false, pw, &mut since, out);
        }
        if row_break(gw, t) {
            since = None;
            out.new_row();
        } else if let Some(e) = since.as_mut() {
            *e += gw;
        }
    }
}

fn dmc(p: &Pulses, t: &Timing, out: &mut BitBuffer) {
    let s = t.short;
    let l = if t.long > 0.0 { t.long } else { 2.0 * s };
    let mid = 0.5 * (s + l);
    // A pending short half-bit.
    let mut half = false;
    let mut symbol = |w: f32, out: &mut BitBuffer| {
        if w > l + 0.5 * s {
            out.new_row();
            half = false;
        } else if w < mid {
            if half {
                out.push(true);
            }
            half = !half;
        } else {
            if half {
                // A lone half-bit: resynchronise.
                out.new_row();
            }
            half = false;
            out.push(false);
        }
    };
    for i in 0..p.n {
        let (pw, gw) = (p.pulse[i] as f32, p.gap[i] as f32);
        if pw > 0.0 {
            symbol(pw, out);
        }
        if row_break(gw, t) {
            out.new_row();
        } else {
            symbol(gw, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing(short: f32, long: f32) -> Timing {
        Timing {
            short,
            long,
            sync: 0.0,
            gap_limit: 0.0,
            reset: 5000.0,
            tolerance: 0.0,
        }
    }

    fn bits(b: &BitBuffer, r: usize) -> String {
        (0..b.bits(r))
            .map(|i| {
                if super::super::bits::bit(b.row(r), i) {
                    '1'
                } else {
                    '0'
                }
            })
            .collect()
    }

    #[test]
    fn pwm_short_is_one() {
        let p = Pulses::from_us(
            250e3,
            &[
                (200.0, 400.0),
                (400.0, 200.0),
                (210.0, 390.0),
                (420.0, 9000.0),
            ],
            false,
        );
        let mut b = BitBuffer::new();
        slice(&p, Modulation::OokPwm, &timing(200.0, 400.0), &mut b);
        assert_eq!(b.num_rows(), 1);
        assert_eq!(bits(&b, 0), "1010");
    }

    #[test]
    fn ppm_long_gap_is_one_and_rows_split() {
        let mut t = timing(1000.0, 2000.0);
        t.gap_limit = 3000.0;
        let p = Pulses::from_us(
            1e6,
            &[
                (500.0, 1000.0),
                (500.0, 2000.0),
                (500.0, 4000.0),
                (500.0, 2000.0),
                (500.0, 9000.0),
            ],
            false,
        );
        let mut b = BitBuffer::new();
        slice(&p, Modulation::OokPpm, &t, &mut b);
        assert_eq!(b.num_rows(), 2);
        assert_eq!(bits(&b, 0), "01");
        assert_eq!(bits(&b, 1), "1");
    }

    #[test]
    fn pcm_nrz() {
        // 1 1 0 1 0 0 0 1 at 100 µs.
        let p = Pulses::from_us(
            1e6,
            &[(200.0, 100.0), (100.0, 300.0), (100.0, 9000.0)],
            true,
        );
        let mut b = BitBuffer::new();
        slice(&p, Modulation::FskPcm, &timing(100.0, 100.0), &mut b);
        assert_eq!(bits(&b, 0), "11010001");
    }

    #[test]
    fn manchester_zerobit() {
        // Bits 0 1 1 0 0, half period 100: levels after the leading 0's
        // rising edge: H | L ... encode: 0 = L→H, 1 = H→L.
        // Halves: 0:(L,H) 1:(H,L) 1:(H,L) 0:(L,H) 0:(L,H)
        // Sequence: L H H L H L L H L H → pulses (from first H): H×2, L×1,
        // H×1, L×2, H×1, L×1, H×1.
        let p = Pulses::from_us(
            1e6,
            &[
                (200.0, 100.0),
                (100.0, 200.0),
                (100.0, 100.0),
                (100.0, 9000.0),
            ],
            false,
        );
        let mut b = BitBuffer::new();
        slice(&p, Modulation::OokManchester, &timing(100.0, 0.0), &mut b);
        assert_eq!(bits(&b, 0), "01100");
    }

    #[test]
    fn dmc_two_halves_are_one() {
        // 1 (two halves), 0 (one long), 1, 0
        let p = Pulses::from_us(
            1e6,
            &[(100.0, 100.0), (200.0, 100.0), (100.0, 200.0)],
            false,
        );
        let mut b = BitBuffer::new();
        let mut t = timing(100.0, 200.0);
        t.reset = 1000.0;
        slice(&p, Modulation::OokDmc, &t, &mut b);
        assert_eq!(bits(&b, 0), "1010");
    }
}
