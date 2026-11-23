//! Broadcast FM extras that work on the multiplex (MPX) signal, i.e. the raw
//! output of an FM discriminator before de-emphasis: the stereo decoder and
//! the RDS decoder.
//!
//! MPX layout (75 kHz deviation = ±1 at `fm_demod`'s output):
//!
//! | band | content |
//! |---|---|
//! | 0–15 kHz | (L+R)/2, pre-emphasized |
//! | 19 kHz | pilot tone, ~10 % of the deviation |
//! | 23–53 kHz | (L−R)/2, DSB-SC on 38 kHz (2× pilot, in phase) |
//! | 57 kHz ± 2.4 kHz | RDS: biphase BPSK, 1187.5 bit/s (57 kHz / 48) |

mod rds;
mod stereo;

use foxygnal_core::{Error, Registry};

use crate::design::fmt_rate;

pub use rds::RdsDecoder;
pub use stereo::FmStereo;

pub fn register(r: &mut Registry) {
    stereo::register(r);
    rds::register(r);
}

/// The pilot tone's frequency.
pub const PILOT: f64 = 19_000.0;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// `(interpolation, decimation)` in lowest terms to go from `from` to `to`
/// samples per second. Both must be whole numbers.
fn ratio(from: f64, to: f64) -> Result<(usize, usize), Error> {
    let whole = |v: f64| v >= 1.0 && v.fract() == 0.0 && v < 1e12;
    if !whole(from) || !whole(to) {
        return Err(Error::Other(format!(
            "can only resample between whole-number rates ({} → {})",
            fmt_rate(from),
            fmt_rate(to)
        )));
    }
    let (a, b) = (to as u64, from as u64);
    let g = gcd(a, b);
    Ok(((a / g) as usize, (b / g) as usize))
}
